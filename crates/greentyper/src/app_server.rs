use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use greentyper_core::agent_team::{
    AgentStatus, Capability, CapabilitySnapshot, CommandOutcome, CompletionCapsule,
    DurableTeamError, ResourceBudget, TaskScope, TeamOperationAcknowledgeOutcome,
    TeamOperationCommit,
};
use greentyper_core::config::{
    CONFIG_FILE_SCHEMA_VERSION, ConfigCommit, ConfigDraft, ConfigErrorCategory, ConfigRuntime,
    ConfigRuntimeError, ConfigScope, ConfigValue, MAX_CONFIG_STRING_BYTES, config_schema,
};
use greentyper_core::model::{DeliveryId, ItemRole, TurnId};
use greentyper_core::pricing::PriceScheduleBook;
use greentyper_core::provider::ProviderRuntime;
use greentyper_core::runtime::{
    AcknowledgeOutcome, CancelTurnOutcome, PreparedOutput, ProviderFallbackCandidate,
    ProviderToolApproval, RecoveryStatus, RuntimeError, RuntimeKernel,
};
use greentyper_core::tool_runtime::{
    AuthorizedToolCall, ToolCallRecord, ToolCallStatus, ToolEffectExecutor, ToolExecution,
    ToolReconciliationDecision, ToolRuntimeError,
};
use greentyper_core::usage::{
    RuntimeUsageQuery, UsageCursor, UsageError, UsageTimestamp, UsageWindow,
};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use serde_json::{Value, json};

use crate::credential_vault::{
    CredentialVault, CredentialVaultError, PlatformCredentialVault, ProviderCredentialScope,
    SecretValue,
};
use crate::local_process::LocalProcessExecutor;
use crate::presentation::AgentCenterView;
use crate::product_driver::{
    ProductDriver, ProductDriverError, ProductInteraction, ProductToolDecision,
    ProductToolDecisionOutcome, acknowledge_product_team_operation,
    apply_model_preset_to_next_turn, cancel_product_agent, cancel_product_provider_turn,
    complete_product_agent, delegate_product_agent, fail_product_agent, freeze_model_selection,
    has_product_driver_state, inspect_product_team, inspect_product_tools,
    message_from_product_agent, reconcile_product_tool,
    request_product_agent_provider_turn_recovery, request_product_provider_turn_recovery,
    require_context_mode_execution, require_pending_context_mode_execution,
};
use crate::provider_http::ConfiguredProvider;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_DRAFTS: usize = 64;

pub(crate) fn run_stdio(
    input: impl BufRead,
    output: impl Write,
    config: ConfigRuntime,
    runtime_path: PathBuf,
) -> Result<(), AppServerError> {
    let mut vault = PlatformCredentialVault;
    run_stdio_with_vault(input, output, config, runtime_path, &mut vault)
}

fn run_stdio_with_vault(
    input: impl BufRead,
    output: impl Write,
    config: ConfigRuntime,
    runtime_path: PathBuf,
    vault: &mut impl CredentialVault,
) -> Result<(), AppServerError> {
    run_stdio_with_vault_and_executor_factory(input, output, config, runtime_path, vault, || {
        LocalProcessExecutor::current()
            .map(|executor| BoxedToolExecutor(Box::new(executor)))
            .map_err(|_| ())
    })
}

fn run_stdio_with_vault_and_executor_factory<F>(
    mut input: impl BufRead,
    mut output: impl Write,
    config: ConfigRuntime,
    runtime_path: PathBuf,
    vault: &mut impl CredentialVault,
    executor_factory: F,
) -> Result<(), AppServerError>
where
    F: FnMut() -> Result<BoxedToolExecutor, ()>,
{
    let mut server = AppServer::new(config, runtime_path, vault, executor_factory);
    loop {
        let response = match read_request_line(&mut input)? {
            RequestLine::End => return Ok(()),
            RequestLine::TooLong => error_response(
                None,
                "request_too_large",
                "request exceeds the maximum frame size",
                None,
            ),
            RequestLine::Value(mut line) => {
                let response = server.handle(&line);
                line.fill(0);
                response
            }
        };
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

struct BoxedToolExecutor(Box<dyn ToolEffectExecutor>);

impl ToolEffectExecutor for BoxedToolExecutor {
    fn execute(&mut self, call: &AuthorizedToolCall<'_>) -> ToolExecution {
        self.0.execute(call)
    }
}

struct ReviewOnlyExecutor;

impl ToolEffectExecutor for ReviewOnlyExecutor {
    fn execute(&mut self, _call: &AuthorizedToolCall<'_>) -> ToolExecution {
        ToolExecution::Failed {
            reason: "Tool review cannot execute an effect".into(),
        }
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ToolReviewBinding {
    call: u64,
    arguments_hash: [u8; 32],
    resources_fingerprint: [u8; 32],
}

struct AppServer<'vault, V, F> {
    config: ConfigRuntime,
    runtime_path: PathBuf,
    drafts: BTreeMap<u64, ConfigDraft>,
    next_draft_id: u64,
    tool_review: Option<ToolReviewBinding>,
    vault: &'vault mut V,
    executor_factory: F,
}

impl<'vault, V, F> AppServer<'vault, V, F>
where
    V: CredentialVault,
    F: FnMut() -> Result<BoxedToolExecutor, ()>,
{
    fn new(
        config: ConfigRuntime,
        runtime_path: PathBuf,
        vault: &'vault mut V,
        executor_factory: F,
    ) -> Self {
        Self {
            config,
            runtime_path,
            drafts: BTreeMap::new(),
            next_draft_id: 1,
            tool_review: None,
            vault,
            executor_factory,
        }
    }

    fn handle(&mut self, line: &[u8]) -> Value {
        let request = match serde_json::from_slice::<Request<'_>>(line) {
            Ok(request) => request,
            Err(_) => {
                return error_response(
                    None,
                    "invalid_request",
                    "request must be a valid JSON object",
                    None,
                );
            }
        };
        if request.operation.len() > 64 || request.operation.chars().any(char::is_control) {
            return error_response(
                Some(request.id),
                "invalid_request",
                "operation is invalid",
                None,
            );
        }
        match request.operation.as_str() {
            "runtime.status" => match parse_params::<EmptyParams>(request.params) {
                Ok(_) => match RuntimeKernel::inspect(&self.runtime_path) {
                    Ok(snapshot) => success_response(request.id, runtime_status(snapshot)),
                    Err(_) => runtime_inspection_error(request.id),
                },
                Err(()) => invalid_params(request.id),
            },
            "runtime.cancel" => {
                let params = match parse_params::<RuntimeTurnParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let turn = match TurnId::new(params.turn) {
                    Ok(turn) => turn,
                    Err(_) => return invalid_turn(request.id),
                };
                let target = match runtime_control_target(&self.runtime_path) {
                    Some(target) => target,
                    None => return runtime_control_error(request.id),
                };
                let outcome = match target {
                    RuntimeControlTarget::Runtime => {
                        let mut runtime =
                            match RuntimeKernel::open_existing_strict(&self.runtime_path) {
                                Ok(runtime) => runtime,
                                Err(_) => return runtime_control_error(request.id),
                            };
                        match runtime.cancel_blocked_turn(turn) {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                return runtime_cancel_error(request.id, error);
                            }
                        }
                    }
                    RuntimeControlTarget::Product => {
                        match cancel_product_provider_turn(&self.runtime_path, turn) {
                            Ok(outcome) => outcome,
                            Err(ProductDriverError::Runtime(error)) => {
                                return runtime_cancel_error(request.id, error);
                            }
                            Err(_) => return runtime_control_error(request.id),
                        }
                    }
                };
                let snapshot = match RuntimeKernel::inspect(&self.runtime_path) {
                    Ok(snapshot) if snapshot.recovered_tail_bytes == 0 => snapshot,
                    Ok(_) | Err(_) => return runtime_inspection_error(request.id),
                };
                success_response(
                    request.id,
                    json!({
                        "status": match outcome {
                            CancelTurnOutcome::Durable(_) => "cancelled",
                            CancelTurnOutcome::AlreadyCancelled => "already_cancelled",
                        },
                        "turn": turn.get(),
                        "ledger": {
                            "transaction": snapshot.head.transaction,
                            "sequence": snapshot.head.sequence,
                        },
                    }),
                )
            }
            "runtime.retry" => {
                let params = match parse_params::<RuntimeTurnParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let turn = match TurnId::new(params.turn) {
                    Ok(turn) => turn,
                    Err(_) => return invalid_turn(request.id),
                };
                if let Err(error) = require_pending_context_mode_execution(&self.runtime_path) {
                    return runtime_retry_error(request.id, error);
                }
                let target = match runtime_control_target(&self.runtime_path) {
                    Some(target) => target,
                    None => return runtime_control_error(request.id),
                };
                match target {
                    RuntimeControlTarget::Runtime => {
                        let mut runtime =
                            match RuntimeKernel::open_existing_strict(&self.runtime_path) {
                                Ok(runtime) => runtime,
                                Err(_) => return runtime_control_error(request.id),
                            };
                        if let Err(error) = runtime.request_blocked_turn_recovery(turn) {
                            return runtime_retry_error(request.id, error);
                        }
                    }
                    RuntimeControlTarget::Product => {
                        match request_product_provider_turn_recovery(&self.runtime_path, turn) {
                            Ok(_) => {}
                            Err(ProductDriverError::Runtime(error)) => {
                                return runtime_retry_error(request.id, error);
                            }
                            Err(_) => return runtime_control_error(request.id),
                        }
                    }
                }
                let snapshot = match RuntimeKernel::inspect(&self.runtime_path) {
                    Ok(snapshot)
                        if snapshot.recovered_tail_bytes == 0
                            && matches!(
                                snapshot.status,
                                RecoveryStatus::ResumeRequired { turn: actual } if actual == turn
                            ) =>
                    {
                        snapshot
                    }
                    Ok(_) | Err(_) => return runtime_inspection_error(request.id),
                };
                success_response(
                    request.id,
                    json!({
                        "status": "resume_required",
                        "turn": turn.get(),
                        "ledger": {
                            "transaction": snapshot.head.transaction,
                            "sequence": snapshot.head.sequence,
                        },
                    }),
                )
            }
            "runtime.resume" => {
                let params = match parse_params::<RuntimeTurnParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let turn = match TurnId::new(params.turn) {
                    Ok(turn) => turn,
                    Err(_) => return invalid_turn(request.id),
                };
                if let Err(error) = require_pending_context_mode_execution(&self.runtime_path) {
                    return runtime_resume_error(request.id, error);
                }
                let target = match runtime_control_target(&self.runtime_path) {
                    Some(target) => target,
                    None => return runtime_control_error(request.id),
                };
                let snapshot = match RuntimeKernel::inspect(&self.runtime_path) {
                    Ok(snapshot)
                        if snapshot.recovered_tail_bytes == 0
                            && matches!(
                                snapshot.status,
                                RecoveryStatus::ResumeRequired { turn: actual } if actual == turn
                            ) =>
                    {
                        snapshot
                    }
                    Ok(_) | Err(_) => return turn_not_resumable(request.id),
                };
                debug_assert!(matches!(
                    snapshot.status,
                    RecoveryStatus::ResumeRequired { turn: actual } if actual == turn
                ));
                match target {
                    RuntimeControlTarget::Runtime => {
                        let mut runtime =
                            match RuntimeKernel::open_existing_strict(&self.runtime_path) {
                                Ok(runtime)
                                    if matches!(
                                        runtime.snapshot().status,
                                        RecoveryStatus::ResumeRequired { turn: actual }
                                            if actual == turn
                                    ) =>
                                {
                                    runtime
                                }
                                Ok(_) => return turn_not_resumable(request.id),
                                Err(_) => return runtime_control_error(request.id),
                            };
                        let mut provider = match runtime.pending_provider_epoch() {
                            Some(epoch) => match ConfiguredProvider::from_epoch(
                                epoch,
                                BorrowedCredentialVault(&mut *self.vault),
                            ) {
                                Ok(provider) => provider,
                                Err(_) => return provider_control_error(request.id),
                            },
                            None => return runtime_control_error(request.id),
                        };
                        match runtime.resume(&mut provider) {
                            Ok(output) => prepared_runtime_response(request.id, &output),
                            Err(error) => runtime_resume_error(request.id, error),
                        }
                    }
                    RuntimeControlTarget::Product => {
                        let mut driver = match ProductDriver::open_existing_for_provider_recovery(
                            &self.runtime_path,
                            turn,
                            BoxedToolExecutor(Box::new(ReviewOnlyExecutor)),
                        ) {
                            Ok(driver) => driver,
                            Err(ProductDriverError::Runtime(RuntimeError::Busy(_))) => {
                                return turn_not_resumable(request.id);
                            }
                            Err(_) => return runtime_control_error(request.id),
                        };
                        let mut provider = match driver.pending_provider_epoch() {
                            Some(epoch) => match ConfiguredProvider::from_epoch(
                                epoch,
                                BorrowedCredentialVault(&mut *self.vault),
                            ) {
                                Ok(provider) => provider,
                                Err(_) => return provider_control_error(request.id),
                            },
                            None => return runtime_control_error(request.id),
                        };
                        provider.enable_local_echo();
                        let mut interaction = AppServerProductInteraction;
                        let result = driver.resume(&mut provider, &mut interaction);
                        drop(provider);
                        drop(driver);
                        match result {
                            Ok(output) => prepared_runtime_response(request.id, &output),
                            Err(ProductDriverError::Interaction(_)) => {
                                let call = inspect_product_tools(&self.runtime_path)
                                    .ok()
                                    .filter(|tools| tools.recovered_tail_bytes == 0)
                                    .and_then(|tools| {
                                        tools.calls.into_iter().find(|record| {
                                            record.status == ToolCallStatus::AwaitingApproval
                                        })
                                    });
                                match call {
                                    Some(call) => success_response(
                                        request.id,
                                        json!({
                                            "status": "tool_approval_required",
                                            "turn": turn.get(),
                                            "call": call.call.get(),
                                        }),
                                    ),
                                    None => runtime_control_error(request.id),
                                }
                            }
                            Err(ProductDriverError::Runtime(error)) => {
                                runtime_resume_error(request.id, error)
                            }
                            Err(_) => runtime_control_error(request.id),
                        }
                    }
                }
            }
            "runtime.delivery" => {
                let params = match parse_params::<RuntimeDeliveryParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let delivery = match DeliveryId::new(params.delivery) {
                    Ok(delivery) => delivery,
                    Err(_) => {
                        return error_response(
                            Some(request.id),
                            "invalid_value",
                            "delivery must be a positive identifier",
                            None,
                        );
                    }
                };
                let snapshot = match RuntimeKernel::inspect(&self.runtime_path) {
                    Ok(snapshot) if snapshot.recovered_tail_bytes == 0 => snapshot,
                    Ok(_) | Err(_) => return runtime_inspection_error(request.id),
                };
                let (turn, pending_delivery) = match snapshot.status {
                    RecoveryStatus::ReconciliationRequired { turn, delivery } => (turn, delivery),
                    _ => return unknown_delivery(request.id),
                };
                if pending_delivery != delivery {
                    return unknown_delivery(request.id);
                }
                let Some(output) = snapshot
                    .items
                    .iter()
                    .rev()
                    .find(|item| item.turn() == turn && item.role() == ItemRole::Assistant)
                else {
                    return runtime_inspection_error(request.id);
                };
                success_response(
                    request.id,
                    json!({
                        "status": "prepared",
                        "delivery": delivery.get(),
                        "turn": turn.get(),
                        "text": output.text(),
                    }),
                )
            }
            "runtime.acknowledge" => {
                let params = match parse_params::<RuntimeAcknowledgeParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let delivery = match DeliveryId::new(params.delivery) {
                    Ok(delivery) => delivery,
                    Err(_) => {
                        return error_response(
                            Some(request.id),
                            "invalid_value",
                            "delivery must be a positive identifier",
                            None,
                        );
                    }
                };
                let snapshot = match RuntimeKernel::inspect(&self.runtime_path) {
                    Ok(snapshot) if snapshot.recovered_tail_bytes == 0 => snapshot,
                    Ok(_) | Err(_) => return runtime_control_error(request.id),
                };
                let pending_turn = match snapshot.status {
                    RecoveryStatus::ReconciliationRequired {
                        turn,
                        delivery: pending,
                    } if pending == delivery => Some(turn),
                    RecoveryStatus::Ready => None,
                    _ => return unknown_delivery(request.id),
                };
                let product_state = match has_product_driver_state(&self.runtime_path) {
                    Ok(product_state) => product_state,
                    Err(_) => return runtime_control_error(request.id),
                };
                let (status, head) = if let (true, Some(turn)) = (product_state, pending_turn) {
                    let mut driver = match ProductDriver::open_existing_for_delivery(
                        &self.runtime_path,
                        turn,
                        BoxedToolExecutor(Box::new(ReviewOnlyExecutor)),
                    ) {
                        Ok(driver) => driver,
                        Err(_) => return runtime_control_error(request.id),
                    };
                    let status = match driver.acknowledge(delivery) {
                        Ok(AcknowledgeOutcome::Durable(_)) => "acknowledged",
                        Ok(AcknowledgeOutcome::AlreadyAcknowledged) => "already_acknowledged",
                        Err(ProductDriverError::Runtime(RuntimeError::UnknownDelivery(_))) => {
                            return unknown_delivery(request.id);
                        }
                        Err(_) => return runtime_control_error(request.id),
                    };
                    let head = RuntimeKernel::inspect(&self.runtime_path)
                        .map(|snapshot| snapshot.head)
                        .unwrap_or(snapshot.head);
                    (status, head)
                } else {
                    let mut runtime = match RuntimeKernel::open_existing_strict(&self.runtime_path)
                    {
                        Ok(runtime) => runtime,
                        Err(_) => return runtime_control_error(request.id),
                    };
                    let status = match runtime.acknowledge(delivery) {
                        Ok(AcknowledgeOutcome::Durable(_)) => "acknowledged",
                        Ok(AcknowledgeOutcome::AlreadyAcknowledged) => "already_acknowledged",
                        Err(RuntimeError::UnknownDelivery(_)) => {
                            return unknown_delivery(request.id);
                        }
                        Err(_) => return runtime_control_error(request.id),
                    };
                    (status, runtime.snapshot().head)
                };
                success_response(
                    request.id,
                    json!({
                        "status": status,
                        "delivery": delivery.get(),
                        "ledger": {
                            "transaction": head.transaction,
                            "sequence": head.sequence,
                        },
                    }),
                )
            }
            "runtime.stats" => {
                let params = match parse_params::<RuntimeStatsParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let (as_of, query) = match runtime_stats_query(params) {
                    Ok(query) => query,
                    Err(error) => return usage_error_response(request.id, error),
                };
                match RuntimeKernel::inspect_usage_report(&self.runtime_path, as_of, query) {
                    Ok(report) => success_response(request.id, json!(report)),
                    Err(error) => runtime_usage_error_response(request.id, error),
                }
            }
            "agent.list" => match parse_params::<EmptyParams>(request.params) {
                Ok(_) => match inspect_product_team(&self.runtime_path) {
                    Ok(Some(team)) => {
                        let pending_operations = team
                            .operations
                            .iter()
                            .filter(|record| {
                                record.status
                                    == greentyper_core::agent_team::TeamOperationStatus::CommittedAwaitingAcknowledgement
                            })
                            .map(team_operation_record)
                            .collect::<Vec<_>>();
                        success_response(
                            request.id,
                            json!({
                                "available": true,
                                "team": AgentCenterView::from(&team),
                                "pending_operations": pending_operations,
                            }),
                        )
                    }
                    Ok(None) => success_response(
                        request.id,
                        json!({
                            "available": false,
                            "team": Value::Null,
                            "pending_operations": [],
                        }),
                    ),
                    Err(_) => team_inspection_error(request.id),
                },
                Err(()) => invalid_params(request.id),
            },
            "agent.delegate" => {
                let params = match parse_params::<AgentDelegateParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let scope = params.scope.map(TaskScope::from_labels);
                let capabilities = CapabilitySnapshot::from_capabilities(
                    params.capabilities.into_iter().map(Capability::from),
                );
                let inherited_model_preset = match self.config.default_model_preset() {
                    Ok(preset) => preset.map(str::to_owned),
                    Err(error) => return config_error_response(request.id, &error),
                };
                match delegate_product_agent(
                    &self.runtime_path,
                    params.parent,
                    params.title,
                    scope,
                    ResourceBudget::new(params.token_budget, params.tool_budget),
                    capabilities,
                    inherited_model_preset.as_deref(),
                ) {
                    Ok(commit) => team_operation_response(request.id, &commit),
                    Err(error) => team_control_error(request.id, error),
                }
            }
            "agent.turn" => {
                let params = match parse_params::<AgentTurnParams>(request.params) {
                    Ok(params) if !params.input.is_empty() => params,
                    Ok(_) | Err(()) => return invalid_params(request.id),
                };
                let executor = match (self.executor_factory)() {
                    Ok(executor) => executor,
                    Err(()) => {
                        return error_response(
                            Some(request.id),
                            "tool_execution_unavailable",
                            "local Tool execution is unavailable",
                            None,
                        );
                    }
                };
                let mut driver = match ProductDriver::open_existing_for_agent(
                    &self.runtime_path,
                    params.agent,
                    executor,
                ) {
                    Ok(driver) => driver,
                    Err(error) => return team_control_error(request.id, error),
                };
                let Some(preset_id) = driver.inherited_model_preset().map(str::to_owned) else {
                    return error_response(
                        Some(request.id),
                        "agent_preset_unavailable",
                        "Agent has no inherited Model Preset",
                        None,
                    );
                };
                let plan = match build_agent_provider_plan(
                    &self.config,
                    &preset_id,
                    SharedCredentialVault(&*self.vault),
                ) {
                    Ok(plan) => plan,
                    Err(AgentProviderPlanError::Config(error)) => {
                        return config_error_response(request.id, &error);
                    }
                    Err(AgentProviderPlanError::Runtime(error)) => {
                        return runtime_resume_error(request.id, error);
                    }
                    Err(AgentProviderPlanError::Provider) => {
                        return provider_control_error(request.id);
                    }
                };
                let AgentProviderPlan {
                    candidates,
                    mut providers,
                    usage_windows,
                    price_schedules,
                } = plan;
                for provider in &mut providers {
                    provider.enable_local_echo();
                }
                let mut interaction = AppServerProductInteraction;
                let result = driver.execute_with_provider_fallbacks(
                    &candidates,
                    usage_windows,
                    price_schedules,
                    params.input,
                    &mut providers,
                    &mut interaction,
                );
                drop(providers);
                drop(driver);
                match result {
                    Ok(output) => prepared_agent_response(request.id, params.agent, &output),
                    Err(ProductDriverError::Interaction(_)) => {
                        let call = inspect_product_tools(&self.runtime_path)
                            .ok()
                            .filter(|tools| tools.recovered_tail_bytes == 0)
                            .and_then(|tools| {
                                tools.calls.into_iter().find(|record| {
                                    record.status == ToolCallStatus::AwaitingApproval
                                })
                            });
                        match call {
                            Some(call) => success_response(
                                request.id,
                                json!({
                                    "status": "tool_approval_required",
                                    "agent": params.agent,
                                    "call": call.call.get(),
                                }),
                            ),
                            None => runtime_control_error(request.id),
                        }
                    }
                    Err(ProductDriverError::Runtime(error)) => {
                        runtime_resume_error(request.id, error)
                    }
                    Err(_) => runtime_control_error(request.id),
                }
            }
            "agent.retry" => {
                let params = match parse_params::<AgentRetryParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let turn = match TurnId::new(params.turn) {
                    Ok(turn) => turn,
                    Err(_) => return invalid_turn(request.id),
                };
                if let Err(error) = require_pending_context_mode_execution(&self.runtime_path) {
                    return runtime_retry_error(request.id, error);
                }
                match request_product_agent_provider_turn_recovery(
                    &self.runtime_path,
                    params.agent,
                    turn,
                ) {
                    Ok(_) => {
                        let snapshot = match RuntimeKernel::inspect(&self.runtime_path) {
                            Ok(snapshot)
                                if snapshot.recovered_tail_bytes == 0
                                    && matches!(
                                        snapshot.status,
                                        RecoveryStatus::ResumeRequired { turn: actual }
                                            if actual == turn
                                    ) =>
                            {
                                snapshot
                            }
                            Ok(_) | Err(_) => return runtime_inspection_error(request.id),
                        };
                        success_response(
                            request.id,
                            json!({
                                "status": "resume_required",
                                "agent": params.agent,
                                "turn": turn.get(),
                                "ledger": {
                                    "transaction": snapshot.head.transaction,
                                    "sequence": snapshot.head.sequence,
                                },
                            }),
                        )
                    }
                    Err(ProductDriverError::Runtime(error)) => {
                        runtime_retry_error(request.id, error)
                    }
                    Err(ProductDriverError::UnknownAgent(_)) => {
                        error_response(Some(request.id), "unknown_agent", "Agent is unknown", None)
                    }
                    Err(_) => runtime_control_error(request.id),
                }
            }
            "agent.message" => {
                let params = match parse_params::<AgentMessageParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                match message_from_product_agent(
                    &self.runtime_path,
                    params.agent,
                    params.recipient,
                    params.body,
                ) {
                    Ok(commit) => team_operation_response(request.id, &commit),
                    Err(error) => team_control_error(request.id, error),
                }
            }
            "agent.complete" => {
                let params = match parse_params::<AgentCompleteParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let capsule = CompletionCapsule {
                    outcome: params.outcome,
                    evidence: params.evidence,
                    changes: params.changes,
                    tests: params.tests,
                    decisions: params.decisions,
                    blockers: params.blockers,
                    artifacts: params.artifacts,
                    residual_risks: params.residual_risks,
                };
                match complete_product_agent(&self.runtime_path, params.agent, capsule) {
                    Ok(commit) => team_operation_response(request.id, &commit),
                    Err(error) => team_control_error(request.id, error),
                }
            }
            "agent.fail" | "agent.cancel" => {
                let params = match parse_params::<AgentTerminalParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let result = if request.operation == "agent.fail" {
                    fail_product_agent(&self.runtime_path, params.agent, params.reason)
                } else {
                    cancel_product_agent(&self.runtime_path, params.agent, params.reason)
                };
                match result {
                    Ok(commit) => team_operation_response(request.id, &commit),
                    Err(error) => team_control_error(request.id, error),
                }
            }
            "agent.acknowledge" => {
                let params = match parse_params::<AgentOperationParams>(request.params) {
                    Ok(params) if params.operation > 0 => params,
                    Ok(_) | Err(()) => return invalid_params(request.id),
                };
                match acknowledge_product_team_operation(&self.runtime_path, params.operation) {
                    Ok(outcome) => {
                        let status = match outcome {
                            TeamOperationAcknowledgeOutcome::Durable(_) => "acknowledged",
                            TeamOperationAcknowledgeOutcome::AlreadyAcknowledged => {
                                "already_acknowledged"
                            }
                        };
                        let team = match inspect_product_team(&self.runtime_path) {
                            Ok(Some(team)) => team,
                            Ok(None) | Err(_) => return team_inspection_error(request.id),
                        };
                        success_response(
                            request.id,
                            json!({
                                "status": status,
                                "operation": params.operation,
                                "ledger": {
                                    "transaction": team.ledger_head.transaction,
                                    "sequence": team.ledger_head.sequence,
                                },
                            }),
                        )
                    }
                    Err(error) => team_control_error(request.id, error),
                }
            }
            "tool.status" => match parse_params::<EmptyParams>(request.params) {
                Ok(_) => match inspect_product_tools(&self.runtime_path) {
                    Ok(snapshot) => {
                        let calls = snapshot
                            .calls
                            .iter()
                            .map(tool_status_record)
                            .collect::<Vec<_>>();
                        success_response(
                            request.id,
                            json!({
                                "ledger": {
                                    "transaction": snapshot.ledger_head.transaction,
                                    "sequence": snapshot.ledger_head.sequence,
                                },
                                "recovered_tail_bytes": snapshot.recovered_tail_bytes,
                                "calls": calls,
                            }),
                        )
                    }
                    Err(_) => tool_inspection_error(request.id),
                },
                Err(()) => invalid_params(request.id),
            },
            "tool.reconcile" => {
                let params = match parse_params::<ToolReconcileParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let (call, decision) = match params {
                    ToolReconcileParams::Succeeded {
                        call,
                        result_sha256,
                    } => {
                        let Some(result_digest) = decode_sha256(&result_sha256) else {
                            return error_response(
                                Some(request.id),
                                "invalid_value",
                                "result_sha256 must be 64 lowercase hexadecimal characters",
                                None,
                            );
                        };
                        (
                            call,
                            ToolReconciliationDecision::ObservedSucceeded { result_digest },
                        )
                    }
                    ToolReconcileParams::Failed { call } => (
                        call,
                        ToolReconciliationDecision::ObservedFailed {
                            reason: "App Server client observed Tool effect failure".into(),
                        },
                    ),
                };
                if call == 0 {
                    return error_response(
                        Some(request.id),
                        "invalid_value",
                        "call must be a positive identifier",
                        None,
                    );
                }
                if !product_control_ready(&self.runtime_path) {
                    return tool_control_unavailable(request.id);
                }
                match reconcile_product_tool(&self.runtime_path, call, decision) {
                    Ok(record) => success_response(request.id, tool_status_record(&record)),
                    Err(error) => tool_control_error(request.id, error),
                }
            }
            "tool.decide" => {
                let params = match parse_params::<ToolDecisionParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let call = params.call();
                if call == 0 {
                    return error_response(
                        Some(request.id),
                        "invalid_value",
                        "call must be a positive identifier",
                        None,
                    );
                }
                if matches!(&params, ToolDecisionParams::Review { .. }) {
                    self.tool_review = None;
                    if !product_control_ready(&self.runtime_path) {
                        return tool_control_unavailable(request.id);
                    }
                    let mut driver = match ProductDriver::open_existing_for_tool_recovery(
                        &self.runtime_path,
                        call,
                        BoxedToolExecutor(Box::new(ReviewOnlyExecutor)),
                    ) {
                        Ok(driver) => driver,
                        Err(error) => return tool_control_error(request.id, error),
                    };
                    let mut provider = match driver.pending_provider_epoch() {
                        Some(epoch) => match ConfiguredProvider::from_epoch(
                            epoch,
                            BorrowedCredentialVault(&mut *self.vault),
                        ) {
                            Ok(provider) => provider,
                            Err(_) => return provider_control_error(request.id),
                        },
                        None => return tool_not_awaiting_approval(request.id),
                    };
                    provider.enable_local_echo();
                    let approval = match driver.recover_pending_tool_approval(call, &mut provider) {
                        Ok(approval) => approval,
                        Err(error) => return tool_control_error(request.id, error),
                    };
                    let binding = tool_review_binding(&approval);
                    let response = tool_review_response(request.id, &approval, binding);
                    self.tool_review = Some(binding);
                    return response;
                }

                let (decision, arguments_sha256, resources_sha256) = match params {
                    ToolDecisionParams::Approve {
                        arguments_sha256,
                        resources_sha256,
                        ..
                    } => (
                        ProductToolDecision::Approve,
                        arguments_sha256,
                        resources_sha256,
                    ),
                    ToolDecisionParams::Deny {
                        arguments_sha256,
                        resources_sha256,
                        ..
                    } => (
                        ProductToolDecision::Deny,
                        arguments_sha256,
                        resources_sha256,
                    ),
                    ToolDecisionParams::Review { .. } => unreachable!("handled above"),
                };
                let Some(arguments_hash) = decode_sha256(&arguments_sha256) else {
                    return invalid_confirmation(request.id);
                };
                let Some(resources_fingerprint) = decode_sha256(&resources_sha256) else {
                    return invalid_confirmation(request.id);
                };
                let provided = ToolReviewBinding {
                    call,
                    arguments_hash,
                    resources_fingerprint,
                };
                let Some(reviewed) = self.tool_review else {
                    return tool_review_required(request.id);
                };
                if reviewed != provided {
                    return tool_review_mismatch(request.id);
                }
                self.tool_review = None;
                if !product_control_ready(&self.runtime_path) {
                    return tool_control_unavailable(request.id);
                }
                let executor = match (self.executor_factory)() {
                    Ok(executor) => executor,
                    Err(()) => {
                        return error_response(
                            Some(request.id),
                            "tool_execution_unavailable",
                            "local Tool execution is unavailable",
                            None,
                        );
                    }
                };
                let mut driver = match ProductDriver::open_existing_for_tool_recovery(
                    &self.runtime_path,
                    call,
                    executor,
                ) {
                    Ok(driver) => driver,
                    Err(error) => return tool_control_error(request.id, error),
                };
                let mut provider = match driver.pending_provider_epoch() {
                    Some(epoch) => match ConfiguredProvider::from_epoch(
                        epoch,
                        BorrowedCredentialVault(&mut *self.vault),
                    ) {
                        Ok(provider) => provider,
                        Err(_) => return provider_control_error(request.id),
                    },
                    None => return tool_not_awaiting_approval(request.id),
                };
                provider.enable_local_echo();
                let approval = match driver.recover_pending_tool_approval(call, &mut provider) {
                    Ok(approval) => approval,
                    Err(error) => return tool_control_error(request.id, error),
                };
                if tool_review_binding(&approval) != reviewed {
                    return tool_review_mismatch(request.id);
                }
                match driver.resolve_recovered_tool_approval(approval, decision, &mut provider) {
                    Ok(ProductToolDecisionOutcome::Prepared(output)) => success_response(
                        request.id,
                        json!({
                            "status": "prepared",
                            "call": call,
                            "delivery": output.delivery().get(),
                            "turn": output.turn().get(),
                            "text": output.text(),
                            "usage_record_count": output.usage_records().len(),
                        }),
                    ),
                    Ok(ProductToolDecisionOutcome::Denied) => {
                        success_response(request.id, json!({ "status": "denied", "call": call }))
                    }
                    Err(error) => tool_control_error(request.id, error),
                }
            }
            "config.schema" => match parse_params::<EmptyParams>(request.params) {
                Ok(_) => success_response(
                    request.id,
                    json!({
                        "schema_version": CONFIG_FILE_SCHEMA_VERSION,
                        "entries": config_schema(),
                    }),
                ),
                Err(()) => invalid_params(request.id),
            },
            "config.get" => {
                let params = match parse_params::<GetParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                if !valid_config_path(&params.path) {
                    return error_response(
                        Some(request.id),
                        "invalid_value",
                        "config path is invalid",
                        None,
                    );
                }
                match self.config.get_effective(&params.path) {
                    Ok(entry) => {
                        let status = public_config_status(&self.config);
                        success_response(
                            request.id,
                            json!({
                                "path": params.path,
                                "entry": entry,
                                "status": status,
                            }),
                        )
                    }
                    Err(error) => config_error_response(request.id, &error),
                }
            }
            "config.draft.begin" => {
                let params = match parse_params::<BeginDraftParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                if self.drafts.len() >= MAX_ACTIVE_DRAFTS {
                    return error_response(
                        Some(request.id),
                        "resource_busy",
                        "too many active drafts",
                        None,
                    );
                }
                let draft = match self.config.begin_draft(params.scope.into()) {
                    Ok(draft) => draft,
                    Err(error) => return config_error_response(request.id, &error),
                };
                self.store_draft(request.id, draft)
            }
            "config.starter.begin" => {
                let params = match parse_params::<BeginStarterParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                if self.drafts.len() >= MAX_ACTIVE_DRAFTS {
                    return error_response(
                        Some(request.id),
                        "resource_busy",
                        "too many active drafts",
                        None,
                    );
                }
                let draft = match self.config.begin_model_starter(
                    params.scope.into(),
                    &params.preset,
                    &params.provider,
                    &params.catalog_key,
                ) {
                    Ok(draft) => draft,
                    Err(error) => return config_error_response(request.id, &error),
                };
                self.store_draft(request.id, draft)
            }
            "config.starter.update.begin" => {
                let params = match parse_params::<BeginStarterUpdateParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                if self.drafts.len() >= MAX_ACTIVE_DRAFTS {
                    return error_response(
                        Some(request.id),
                        "resource_busy",
                        "too many active drafts",
                        None,
                    );
                }
                let draft = match self
                    .config
                    .begin_model_starter_update(params.scope.into(), &params.preset)
                {
                    Ok(draft) => draft,
                    Err(error) => return config_error_response(request.id, &error),
                };
                self.store_draft(request.id, draft)
            }
            "config.draft.set" => {
                let params = match parse_params::<SetDraftParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                if !valid_config_path(&params.path) {
                    return error_response(
                        Some(request.id),
                        "invalid_value",
                        "config path is invalid",
                        None,
                    );
                }
                let Some(draft) = self.drafts.get_mut(&params.draft_id) else {
                    return unknown_draft(request.id);
                };
                match draft.set(&params.path, params.value.into()) {
                    Ok(()) => success_response(
                        request.id,
                        json!({ "draft_id": params.draft_id, "staged": true }),
                    ),
                    Err(error) => config_error_response(request.id, &error),
                }
            }
            "config.draft.reset" => {
                let params = match parse_params::<ResetDraftParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                if !valid_config_path(&params.path) {
                    return error_response(
                        Some(request.id),
                        "invalid_value",
                        "config path is invalid",
                        None,
                    );
                }
                let Some(draft) = self.drafts.get_mut(&params.draft_id) else {
                    return unknown_draft(request.id);
                };
                match draft.reset(&params.path) {
                    Ok(()) => success_response(
                        request.id,
                        json!({ "draft_id": params.draft_id, "staged": true }),
                    ),
                    Err(error) => config_error_response(request.id, &error),
                }
            }
            "config.draft.validate" => {
                let params = match parse_params::<DraftIdParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let Some(draft) = self.drafts.get(&params.draft_id) else {
                    return unknown_draft(request.id);
                };
                match self.config.validate_draft(draft) {
                    Ok(changes) => success_response(
                        request.id,
                        json!({
                            "draft_id": params.draft_id,
                            "base_revision": draft.base_revision().to_string(),
                            "changes": changes,
                        }),
                    ),
                    Err(error) => config_error_response(request.id, &error),
                }
            }
            "config.draft.commit" => {
                let params = match parse_params::<DraftIdParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let Some(draft) = self.drafts.get(&params.draft_id).cloned() else {
                    return unknown_draft(request.id);
                };
                let preview = match self.config.commit(draft.clone(), true) {
                    Ok(preview) => preview,
                    Err(error) => {
                        self.refresh_after_conflict(&error);
                        return config_error_response(request.id, &error);
                    }
                };
                if preview.changes.is_empty() {
                    self.drafts.remove(&params.draft_id);
                    return commit_response(request.id, params.draft_id, preview);
                }
                match self.config.commit(draft, false) {
                    Ok(commit) => {
                        self.drafts.remove(&params.draft_id);
                        commit_response(request.id, params.draft_id, commit)
                    }
                    Err(error) => {
                        self.refresh_after_conflict(&error);
                        config_error_response(request.id, &error)
                    }
                }
            }
            "credential.bind" => {
                let (scope, secret) = match credential_mutation_values(request.params) {
                    Ok(values) => values,
                    Err(CredentialMutationError::InvalidParams) => {
                        return invalid_params(request.id);
                    }
                    Err(CredentialMutationError::Vault(error)) => {
                        return credential_error_response(request.id, error);
                    }
                };
                match self.vault.bind(&scope, secret) {
                    Ok(()) => success_response(request.id, json!({ "status": "bound" })),
                    Err(error) => credential_error_response(request.id, error),
                }
            }
            "credential.replace" => {
                let (scope, secret) = match credential_mutation_values(request.params) {
                    Ok(values) => values,
                    Err(CredentialMutationError::InvalidParams) => {
                        return invalid_params(request.id);
                    }
                    Err(CredentialMutationError::Vault(error)) => {
                        return credential_error_response(request.id, error);
                    }
                };
                match self.vault.replace(&scope, secret) {
                    Ok(()) => success_response(request.id, json!({ "status": "replaced" })),
                    Err(error) => credential_error_response(request.id, error),
                }
            }
            "credential.test" => {
                let params = match parse_params::<CredentialScopeParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let scope = match credential_scope(params) {
                    Ok(scope) => scope,
                    Err(error) => return credential_error_response(request.id, error),
                };
                match self.vault.resolve(&scope) {
                    Ok(secret) => {
                        drop(secret);
                        success_response(request.id, json!({ "status": "available" }))
                    }
                    Err(CredentialVaultError::NotFound) => {
                        success_response(request.id, json!({ "status": "not_found" }))
                    }
                    Err(error) => credential_error_response(request.id, error),
                }
            }
            "credential.forget" => {
                let params = match parse_params::<CredentialScopeParams>(request.params) {
                    Ok(params) => params,
                    Err(()) => return invalid_params(request.id),
                };
                let scope = match credential_scope(params) {
                    Ok(scope) => scope,
                    Err(error) => return credential_error_response(request.id, error),
                };
                match self.vault.forget(&scope) {
                    Ok(true) => success_response(request.id, json!({ "status": "forgotten" })),
                    Ok(false) => success_response(request.id, json!({ "status": "not_found" })),
                    Err(error) => credential_error_response(request.id, error),
                }
            }
            _ => error_response(
                Some(request.id),
                "unknown_operation",
                "operation is not supported",
                None,
            ),
        }
    }

    fn refresh_after_conflict(&mut self, error: &ConfigRuntimeError) {
        if matches!(error, ConfigRuntimeError::RevisionConflict { .. }) {
            let _ = self.config.reload();
        }
    }

    fn store_draft(&mut self, request_id: u64, draft: ConfigDraft) -> Value {
        let Some(next_draft_id) = self.next_draft_id.checked_add(1) else {
            return error_response(
                Some(request_id),
                "resource_busy",
                "draft identifiers are exhausted",
                None,
            );
        };
        let draft_id = self.next_draft_id;
        self.next_draft_id = next_draft_id;
        let scope = draft.scope();
        let base_revision = draft.base_revision().to_string();
        self.drafts.insert(draft_id, draft);
        success_response(
            request_id,
            json!({
                "draft_id": draft_id,
                "scope": scope,
                "base_revision": base_revision,
            }),
        )
    }
}

struct BorrowedCredentialVault<'vault, V>(&'vault mut V);

impl<V: CredentialVault> CredentialVault for BorrowedCredentialVault<'_, V> {
    fn bind(
        &mut self,
        scope: &ProviderCredentialScope,
        secret: SecretValue,
    ) -> Result<(), CredentialVaultError> {
        self.0.bind(scope, secret)
    }

    fn replace(
        &mut self,
        scope: &ProviderCredentialScope,
        secret: SecretValue,
    ) -> Result<(), CredentialVaultError> {
        self.0.replace(scope, secret)
    }

    fn resolve(
        &self,
        scope: &ProviderCredentialScope,
    ) -> Result<SecretValue, CredentialVaultError> {
        self.0.resolve(scope)
    }

    fn forget(&mut self, scope: &ProviderCredentialScope) -> Result<bool, CredentialVaultError> {
        self.0.forget(scope)
    }
}

#[derive(Clone, Copy)]
struct SharedCredentialVault<'vault, V>(&'vault V);

impl<V: CredentialVault> CredentialVault for SharedCredentialVault<'_, V> {
    fn bind(
        &mut self,
        _scope: &ProviderCredentialScope,
        _secret: SecretValue,
    ) -> Result<(), CredentialVaultError> {
        Err(CredentialVaultError::Unavailable)
    }

    fn replace(
        &mut self,
        _scope: &ProviderCredentialScope,
        _secret: SecretValue,
    ) -> Result<(), CredentialVaultError> {
        Err(CredentialVaultError::Unavailable)
    }

    fn resolve(
        &self,
        scope: &ProviderCredentialScope,
    ) -> Result<SecretValue, CredentialVaultError> {
        self.0.resolve(scope)
    }

    fn forget(&mut self, _scope: &ProviderCredentialScope) -> Result<bool, CredentialVaultError> {
        Err(CredentialVaultError::Unavailable)
    }
}

struct AgentProviderPlan<P> {
    candidates: Vec<ProviderFallbackCandidate>,
    providers: Vec<P>,
    usage_windows: Vec<UsageWindow>,
    price_schedules: PriceScheduleBook,
}

enum AgentProviderPlanError {
    Config(ConfigRuntimeError),
    Runtime(RuntimeError),
    Provider,
}

fn build_agent_provider_plan<'vault, V: CredentialVault>(
    config: &ConfigRuntime,
    preset_id: &str,
    vault: SharedCredentialVault<'vault, V>,
) -> Result<
    AgentProviderPlan<ConfiguredProvider<SharedCredentialVault<'vault, V>>>,
    AgentProviderPlanError,
> {
    let presets = config
        .model_preset_chain(preset_id)
        .map_err(AgentProviderPlanError::Config)?;
    let base_layers = config
        .config_layers()
        .map_err(AgentProviderPlanError::Config)?
        .clone();
    let usage_windows = config
        .resolved_usage_windows()
        .map_err(AgentProviderPlanError::Config)?;
    let price_schedules = config
        .resolved_price_schedules()
        .map_err(AgentProviderPlanError::Config)?;
    let mut preflight = Vec::with_capacity(presets.len());
    for preset in &presets {
        let mut layers = base_layers.clone();
        apply_model_preset_to_next_turn(&mut layers, preset);
        require_context_mode_execution(&layers).map_err(AgentProviderPlanError::Runtime)?;
        let model = layers
            .resolve()
            .map_err(|_| AgentProviderPlanError::Provider)?
            .provider_model()
            .value()
            .clone();
        preflight.push((layers, model));
    }

    let mut candidates = Vec::with_capacity(presets.len());
    let mut providers = Vec::with_capacity(presets.len());
    for (preset, (layers, model)) in presets.iter().zip(preflight) {
        let profile = config
            .provider_profile(&preset.provider)
            .map_err(AgentProviderPlanError::Config)?;
        let provider = match profile {
            Some(profile) => ConfiguredProvider::for_new_turn_with_preferred_dialect(
                profile,
                &model,
                preset.dialect,
                SharedCredentialVault(vault.0),
            ),
            // A simulator preset still carries a schema dialect for selection
            // identity, but its runtime deliberately has no wire dialect or
            // credential boundary.
            None => ConfiguredProvider::for_new_turn(None, SharedCredentialVault(vault.0)),
        }
        .map_err(|_| AgentProviderPlanError::Provider)?;
        let selection = freeze_model_selection(&layers, &usage_windows, &price_schedules, preset)
            .map_err(AgentProviderPlanError::Runtime)?;
        candidates.push(
            ProviderFallbackCandidate::new(
                selection,
                layers,
                provider.profile_snapshot().cloned(),
                provider.dialect(),
            )
            .map_err(AgentProviderPlanError::Runtime)?,
        );
        providers.push(provider);
    }
    Ok(AgentProviderPlan {
        candidates,
        providers,
        usage_windows,
        price_schedules,
    })
}

struct AppServerProductInteraction;

impl ProductInteraction for AppServerProductInteraction {
    fn present_team_operation(
        &mut self,
        _record: greentyper_core::agent_team::TeamOperationRecord,
    ) -> io::Result<()> {
        Err(io::Error::other(
            "App Server cannot acknowledge an unpresented Team operation",
        ))
    }

    fn decide_tool(
        &mut self,
        _approval: &greentyper_core::runtime::ProviderToolApproval,
    ) -> io::Result<ProductToolDecision> {
        Err(io::Error::other(
            "App Server Tool decisions require an explicit request",
        ))
    }
}

fn runtime_status(snapshot: greentyper_core::runtime::RuntimeSnapshot) -> Value {
    let (status, turn, delivery, retryable) = match snapshot.status {
        RecoveryStatus::Ready => ("ready", None, None, false),
        RecoveryStatus::ResumeRequired { turn } => {
            ("resume_required", Some(turn.get()), None, false)
        }
        RecoveryStatus::ReconciliationRequired { turn, delivery } => (
            "reconciliation_required",
            Some(turn.get()),
            Some(delivery.get()),
            false,
        ),
        RecoveryStatus::Blocked {
            turn, retryable, ..
        } => ("blocked", Some(turn.get()), None, retryable),
    };
    json!({
        "ledger": {
            "transaction": snapshot.head.transaction,
            "sequence": snapshot.head.sequence,
        },
        "recovered_tail_bytes": snapshot.recovered_tail_bytes,
        "status": status,
        "turn": turn,
        "delivery": delivery,
        "retryable": retryable,
        "thread": snapshot.thread.map(|thread| thread.get()),
        "item_count": snapshot.items.len(),
        "pending_model_selection": snapshot.pending_model_selection.is_some(),
    })
}

fn runtime_inspection_error(id: u64) -> Value {
    error_response(
        Some(id),
        "runtime_unavailable",
        "Runtime state could not be inspected",
        None,
    )
}

fn runtime_control_error(id: u64) -> Value {
    error_response(
        Some(id),
        "runtime_unavailable",
        "Runtime state could not be changed",
        None,
    )
}

#[derive(Clone, Copy)]
enum RuntimeControlTarget {
    Runtime,
    Product,
}

fn runtime_control_target(runtime_path: &PathBuf) -> Option<RuntimeControlTarget> {
    if fs::symlink_metadata(runtime_path).is_err()
        || !RuntimeKernel::inspect(runtime_path)
            .is_ok_and(|snapshot| snapshot.recovered_tail_bytes == 0)
    {
        return None;
    }
    match has_product_driver_state(runtime_path) {
        Ok(false) => Some(RuntimeControlTarget::Runtime),
        Ok(true) if product_control_ready(runtime_path) => Some(RuntimeControlTarget::Product),
        Ok(true) | Err(_) => None,
    }
}

fn runtime_cancel_error(id: u64, error: RuntimeError) -> Value {
    match error {
        RuntimeError::UnknownTurn(_) => {
            error_response(Some(id), "unknown_turn", "Runtime Turn is unknown", None)
        }
        RuntimeError::TurnCancellationNotAllowed(_) => error_response(
            Some(id),
            "turn_not_cancellable",
            "Runtime Turn cannot be cancelled",
            None,
        ),
        _ => runtime_control_error(id),
    }
}

fn runtime_retry_error(id: u64, error: RuntimeError) -> Value {
    match error {
        RuntimeError::UnknownTurn(_) => {
            error_response(Some(id), "unknown_turn", "Runtime Turn is unknown", None)
        }
        RuntimeError::TurnRetryNotAllowed(_) => error_response(
            Some(id),
            "turn_not_retryable",
            "Runtime Turn cannot be retried",
            None,
        ),
        _ => runtime_control_error(id),
    }
}

fn runtime_resume_error(id: u64, error: RuntimeError) -> Value {
    match error {
        RuntimeError::Busy(_) | RuntimeError::UnknownTurn(_) => turn_not_resumable(id),
        RuntimeError::Provider(_) | RuntimeError::InvalidProviderOutput(_) => {
            provider_control_error(id)
        }
        _ => runtime_control_error(id),
    }
}

fn turn_not_resumable(id: u64) -> Value {
    error_response(
        Some(id),
        "turn_not_resumable",
        "Runtime Turn is not awaiting resume",
        None,
    )
}

fn prepared_runtime_response(id: u64, output: &PreparedOutput) -> Value {
    prepared_response(id, None, output)
}

fn prepared_agent_response(id: u64, agent: u64, output: &PreparedOutput) -> Value {
    prepared_response(id, Some(agent), output)
}

fn prepared_response(id: u64, agent: Option<u64>, output: &PreparedOutput) -> Value {
    let mut result = json!({
        "status": "prepared",
        "delivery": output.delivery().get(),
        "turn": output.turn().get(),
        "text": output.text(),
        "usage_record_count": output.usage_records().len(),
    });
    if let Some(agent) = agent {
        result["agent"] = json!(agent);
    }
    success_response(id, result)
}

fn invalid_turn(id: u64) -> Value {
    error_response(
        Some(id),
        "invalid_value",
        "turn must be a positive identifier",
        None,
    )
}

fn tool_review_binding(approval: &ProviderToolApproval) -> ToolReviewBinding {
    ToolReviewBinding {
        call: approval.call().get(),
        arguments_hash: approval.arguments_hash().bytes(),
        resources_fingerprint: approval.resources().binding().fingerprint(),
    }
}

fn tool_review_response(
    id: u64,
    approval: &ProviderToolApproval,
    binding: ToolReviewBinding,
) -> Value {
    let arguments = serde_json::from_str::<Value>(approval.arguments().canonical_json())
        .expect("canonical Tool arguments are valid JSON");
    let resources = approval.resources();
    success_response(
        id,
        json!({
            "status": "review_required",
            "call": binding.call,
            "tool": approval.tool(),
            "arguments": arguments,
            "resources": {
                "filesystem_reads": resources.filesystem_reads().collect::<Vec<_>>(),
                "filesystem_writes": resources.filesystem_writes().collect::<Vec<_>>(),
                "process": resources.process(),
                "network_targets": resources.network_targets().collect::<Vec<_>>(),
            },
            "confirmation": {
                "arguments_sha256": encode_sha256(binding.arguments_hash),
                "resources_sha256": encode_sha256(binding.resources_fingerprint),
            },
        }),
    )
}

fn product_control_ready(runtime_path: &PathBuf) -> bool {
    if fs::symlink_metadata(runtime_path).is_err()
        || !RuntimeKernel::inspect(runtime_path)
            .is_ok_and(|snapshot| snapshot.recovered_tail_bytes == 0)
        || !matches!(has_product_driver_state(runtime_path), Ok(true))
    {
        return false;
    }
    let team_ready = inspect_product_team(runtime_path).is_ok_and(|team| {
        team.is_some_and(|team| {
            team.recovered_tail_bytes == 0
                && team
                    .projection
                    .agents
                    .iter()
                    .any(|agent| agent.status == AgentStatus::Active)
        })
    });
    let tools_ready =
        inspect_product_tools(runtime_path).is_ok_and(|tools| tools.recovered_tail_bytes == 0);
    team_ready && tools_ready
}

fn unknown_delivery(id: u64) -> Value {
    error_response(
        Some(id),
        "unknown_delivery",
        "output delivery is not awaiting acknowledgement",
        None,
    )
}

fn team_inspection_error(id: u64) -> Value {
    error_response(
        Some(id),
        "team_unavailable",
        "Agent Team state could not be inspected",
        None,
    )
}

fn team_operation_record(record: &greentyper_core::agent_team::TeamOperationRecord) -> Value {
    json!({
        "operation": record.operation.get(),
        "transaction": record.transaction.get(),
        "first_sequence": record.first_sequence.get(),
        "last_sequence": record.last_sequence.get(),
        "event_count": record.event_count,
    })
}

fn team_operation_response(id: u64, operation: &TeamOperationCommit) -> Value {
    let outcome = match &operation.commit.outcome {
        CommandOutcome::RootAdmitted { task, agent, .. } => json!({
            "kind": "root_admitted",
            "task": task.get(),
            "agent": agent.get(),
        }),
        CommandOutcome::Delegated { task, agent, .. } => json!({
            "kind": "delegated",
            "task": task.get(),
            "agent": agent.get(),
        }),
        CommandOutcome::MessageAccepted { message } => json!({
            "kind": "message_accepted",
            "message": message.get(),
        }),
        CommandOutcome::StateChanged { task, agent } => json!({
            "kind": "state_changed",
            "task": task.get(),
            "agent": agent.get(),
        }),
    };
    success_response(
        id,
        json!({
            "status": "committed_awaiting_acknowledgement",
            "operation": operation.operation.get(),
            "ledger": {
                "transaction": operation.commit.transaction.get(),
                "sequence": operation.commit.revision.get(),
                "event_count": operation.commit.events.len(),
            },
            "outcome": outcome,
        }),
    )
}

fn team_control_error(id: u64, error: ProductDriverError) -> Value {
    match error {
        ProductDriverError::UnknownAgent(_) => {
            error_response(Some(id), "unknown_agent", "Agent is unknown", None)
        }
        ProductDriverError::UnknownTeamOperation(_) => error_response(
            Some(id),
            "unknown_team_operation",
            "Team operation is unknown",
            None,
        ),
        ProductDriverError::Runtime(RuntimeError::TeamOperationReconciliationRequired(
            operation,
        )) => {
            let mut response = error_response(
                Some(id),
                "team_acknowledgement_required",
                "a committed Team operation must be acknowledged first",
                None,
            );
            response["error"]["operation"] = json!(operation.get());
            response
        }
        ProductDriverError::Runtime(RuntimeError::Busy(_)) => error_response(
            Some(id),
            "runtime_busy",
            "the Provider Runtime must be ready before changing Agent Team state",
            None,
        ),
        ProductDriverError::Runtime(RuntimeError::Team(DurableTeamError::Team(_))) => {
            error_response(
                Some(id),
                "invalid_value",
                "Agent Team command is invalid",
                None,
            )
        }
        ProductDriverError::TeamStateUnavailable
        | ProductDriverError::CurrentAgentUnavailable
        | ProductDriverError::UnexpectedRecovery => error_response(
            Some(id),
            "team_unavailable",
            "Agent Team state could not be changed",
            None,
        ),
        _ => error_response(
            Some(id),
            "team_unavailable",
            "Agent Team state could not be changed",
            None,
        ),
    }
}

fn tool_status_record(record: &ToolCallRecord) -> Value {
    json!({
        "call": record.call.get(),
        "agent": record.agent.get(),
        "tool": record.tool,
        "status": match record.status {
            ToolCallStatus::AwaitingApproval => "awaiting_approval",
            ToolCallStatus::Denied => "denied",
            ToolCallStatus::ReconciliationRequired => "reconciliation_required",
            ToolCallStatus::Succeeded => "succeeded",
            ToolCallStatus::Failed => "failed",
        },
        "approval_expires_at_unix_ms": record.approval_expires_at_unix_ms,
        "result_sha256": record.result_digest.map(encode_sha256),
    })
}

fn encode_sha256(digest: [u8; 32]) -> String {
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        fmt::write(&mut encoded, format_args!("{byte:02x}"))
            .expect("writing to a String cannot fail");
    }
    encoded
}

fn decode_sha256(value: &str) -> Option<[u8; 32]> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        digest[index] = (decode_hex_digit(pair[0])? << 4) | decode_hex_digit(pair[1])?;
    }
    Some(digest)
}

const fn decode_hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

fn tool_inspection_error(id: u64) -> Value {
    error_response(
        Some(id),
        "tool_unavailable",
        "Tool state could not be inspected",
        None,
    )
}

fn tool_control_error(id: u64, error: ProductDriverError) -> Value {
    let (category, message) = match error {
        ProductDriverError::UnknownToolCall(_) => ("unknown_tool_call", "Tool call is unknown"),
        ProductDriverError::ToolOwnerUnavailable(_) => (
            "tool_owner_unavailable",
            "Tool call owner cannot be recovered",
        ),
        ProductDriverError::Runtime(RuntimeError::Tool(ToolRuntimeError::InvalidTransition {
            ..
        })) => (
            "tool_not_reconcilable",
            "Tool call is not awaiting reconciliation",
        ),
        ProductDriverError::ToolApprovalUnavailable(_) => {
            return tool_not_awaiting_approval(id);
        }
        ProductDriverError::ToolApprovalMismatch { .. } => (
            "tool_approval_mismatch",
            "Tool call does not match the pending approval",
        ),
        ProductDriverError::Runtime(RuntimeError::Provider(_)) => {
            return provider_control_error(id);
        }
        _ => ("tool_unavailable", "Tool state could not be changed"),
    };
    error_response(Some(id), category, message, None)
}

fn tool_control_unavailable(id: u64) -> Value {
    error_response(
        Some(id),
        "tool_unavailable",
        "Tool state could not be changed",
        None,
    )
}

fn tool_not_awaiting_approval(id: u64) -> Value {
    error_response(
        Some(id),
        "tool_not_awaiting_approval",
        "Tool call is not awaiting approval",
        None,
    )
}

fn invalid_confirmation(id: u64) -> Value {
    error_response(
        Some(id),
        "invalid_value",
        "Tool confirmation hashes must be 64 lowercase hexadecimal characters",
        None,
    )
}

fn tool_review_required(id: u64) -> Value {
    error_response(
        Some(id),
        "tool_review_required",
        "Tool call must be reviewed on this connection before a decision",
        None,
    )
}

fn tool_review_mismatch(id: u64) -> Value {
    error_response(
        Some(id),
        "tool_approval_mismatch",
        "Tool confirmation does not match the reviewed approval",
        None,
    )
}

fn provider_control_error(id: u64) -> Value {
    error_response(
        Some(id),
        "provider_unavailable",
        "frozen Provider state could not be resumed",
        None,
    )
}

fn runtime_stats_query(
    params: RuntimeStatsParams,
) -> Result<(UsageTimestamp, RuntimeUsageQuery), UsageError> {
    let as_of = match params.as_of_unix_ms {
        Some(value) => UsageTimestamp::from_unix_millis(value)?,
        None => UsageTimestamp::now()?,
    };
    let query = match (params.limit, params.cursor) {
        (None, None) => RuntimeUsageQuery::summary_only(),
        (None, Some(_)) => return Err(UsageError::InvalidPageSize),
        (Some(limit), cursor) => {
            let cursor = cursor
                .map(|cursor| cursor.parse::<UsageCursor>())
                .transpose()?;
            RuntimeUsageQuery::page(limit, cursor)?
        }
    };
    Ok((as_of, query))
}

fn runtime_usage_error_response(id: u64, error: RuntimeError) -> Value {
    match error {
        RuntimeError::Usage(error) => usage_error_response(id, error),
        _ => runtime_inspection_error(id),
    }
}

fn usage_error_response(id: u64, error: UsageError) -> Value {
    let (category, message) = match error {
        UsageError::InvalidPageSize => (
            "invalid_value",
            "usage limit must be within the supported range",
        ),
        UsageError::InvalidCursor => ("invalid_value", "usage cursor is invalid"),
        UsageError::StaleCursor => (
            "stale_cursor",
            "usage cursor refers to a stale Runtime revision",
        ),
        UsageError::CursorQueryMismatch => (
            "cursor_query_mismatch",
            "usage cursor does not match the requested instant",
        ),
        UsageError::ClockBeforeUnixEpoch | UsageError::TimestampRange => {
            ("invalid_value", "usage timestamp is invalid")
        }
        _ => ("usage_unavailable", "Usage state could not be inspected"),
    };
    error_response(Some(id), category, message, None)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request<'request> {
    id: u64,
    operation: String,
    #[serde(borrow)]
    params: Option<&'request RawValue>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentDelegateParams {
    parent: Option<u64>,
    title: String,
    scope: Option<Vec<String>>,
    token_budget: u64,
    tool_budget: u32,
    #[serde(default)]
    capabilities: Vec<WireCapability>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTurnParams {
    agent: u64,
    input: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentRetryParams {
    agent: u64,
    turn: u64,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WireCapability {
    WorkspaceRead,
    WorkspaceWrite,
    Process,
    Network,
    Tool { name: String },
}

impl From<WireCapability> for Capability {
    fn from(capability: WireCapability) -> Self {
        match capability {
            WireCapability::WorkspaceRead => Self::WorkspaceRead,
            WireCapability::WorkspaceWrite => Self::WorkspaceWrite,
            WireCapability::Process => Self::Process,
            WireCapability::Network => Self::Network,
            WireCapability::Tool { name } => Self::Tool(name),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentOperationParams {
    operation: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentMessageParams {
    agent: Option<u64>,
    recipient: Option<u64>,
    body: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentCompleteParams {
    agent: Option<u64>,
    outcome: String,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default)]
    changes: Vec<String>,
    #[serde(default)]
    tests: Vec<String>,
    #[serde(default)]
    decisions: Vec<String>,
    #[serde(default)]
    blockers: Vec<String>,
    #[serde(default)]
    artifacts: Vec<String>,
    #[serde(default)]
    residual_risks: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentTerminalParams {
    agent: Option<u64>,
    reason: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeStatsParams {
    as_of_unix_ms: Option<i64>,
    limit: Option<usize>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeAcknowledgeParams {
    delivery: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeDeliveryParams {
    delivery: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeTurnParams {
    turn: u64,
}

#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
enum ToolReconcileParams {
    Succeeded { call: u64, result_sha256: String },
    Failed { call: u64 },
}

#[derive(Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
enum ToolDecisionParams {
    Review {
        call: u64,
    },
    Approve {
        call: u64,
        arguments_sha256: String,
        resources_sha256: String,
    },
    Deny {
        call: u64,
        arguments_sha256: String,
        resources_sha256: String,
    },
}

impl ToolDecisionParams {
    const fn call(&self) -> u64 {
        match self {
            Self::Review { call } | Self::Approve { call, .. } | Self::Deny { call, .. } => *call,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GetParams {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginDraftParams {
    scope: WireConfigScope,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginStarterParams {
    scope: WireConfigScope,
    preset: String,
    provider: String,
    catalog_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BeginStarterUpdateParams {
    scope: WireConfigScope,
    preset: String,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WireConfigScope {
    BuiltIn,
    User,
    Project,
    Cli,
}

impl From<WireConfigScope> for ConfigScope {
    fn from(scope: WireConfigScope) -> Self {
        match scope {
            WireConfigScope::BuiltIn => Self::BuiltIn,
            WireConfigScope::User => Self::User,
            WireConfigScope::Project => Self::Project,
            WireConfigScope::Cli => Self::Cli,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetDraftParams {
    draft_id: u64,
    path: String,
    value: WireConfigValue,
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum WireConfigValue {
    String(String),
    PositiveInteger(u32),
    NonNegativeInteger(u64),
    Boolean(bool),
    StringList(Vec<String>),
}

impl From<WireConfigValue> for ConfigValue {
    fn from(value: WireConfigValue) -> Self {
        match value {
            WireConfigValue::String(value) => Self::String(value),
            WireConfigValue::PositiveInteger(value) => Self::PositiveInteger(value),
            WireConfigValue::NonNegativeInteger(value) => Self::NonNegativeInteger(value),
            WireConfigValue::Boolean(value) => Self::Boolean(value),
            WireConfigValue::StringList(value) => Self::StringList(value),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResetDraftParams {
    draft_id: u64,
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftIdParams {
    draft_id: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialScopeParams {
    reference: String,
    profile: String,
    origin: String,
    #[serde(default)]
    allow_insecure_loopback: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialMutationParams {
    reference: String,
    profile: String,
    origin: String,
    #[serde(default)]
    allow_insecure_loopback: bool,
    secret: WireSecret,
}

struct WireSecret {
    bytes: Option<Vec<u8>>,
}

impl WireSecret {
    fn into_secret(mut self) -> Result<SecretValue, CredentialVaultError> {
        let bytes = self
            .bytes
            .take()
            .ok_or(CredentialVaultError::InvalidSecret)?;
        SecretValue::new(bytes)
    }
}

impl<'de> Deserialize<'de> for WireSecret {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(|secret| Self {
            bytes: Some(secret.into_bytes()),
        })
    }
}

impl Drop for WireSecret {
    fn drop(&mut self) {
        if let Some(bytes) = self.bytes.as_mut() {
            bytes.fill(0);
        }
    }
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Option<&RawValue>) -> Result<T, ()> {
    serde_json::from_str(params.map_or("{}", RawValue::get)).map_err(|_| ())
}

fn valid_config_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= MAX_CONFIG_STRING_BYTES && !path.chars().any(char::is_control)
}

fn credential_scope(
    params: CredentialScopeParams,
) -> Result<ProviderCredentialScope, CredentialVaultError> {
    ProviderCredentialScope::new(
        &params.profile,
        &params.reference,
        &params.origin,
        params.allow_insecure_loopback,
    )
}

enum CredentialMutationError {
    InvalidParams,
    Vault(CredentialVaultError),
}

fn credential_mutation_values(
    params: Option<&RawValue>,
) -> Result<(ProviderCredentialScope, SecretValue), CredentialMutationError> {
    let params = parse_params::<CredentialMutationParams>(params)
        .map_err(|()| CredentialMutationError::InvalidParams)?;
    let CredentialMutationParams {
        reference,
        profile,
        origin,
        allow_insecure_loopback,
        secret,
    } = params;
    let secret = secret
        .into_secret()
        .map_err(CredentialMutationError::Vault)?;
    let scope = credential_scope(CredentialScopeParams {
        reference,
        profile,
        origin,
        allow_insecure_loopback,
    })
    .map_err(CredentialMutationError::Vault)?;
    Ok((scope, secret))
}

fn success_response(id: u64, result: Value) -> Value {
    json!({ "id": id, "result": result })
}

fn public_config_status(config: &ConfigRuntime) -> Value {
    let status = config.status();
    let issues = status
        .issues
        .iter()
        .map(|issue| {
            json!({
                "scope": issue.scope,
                "category": issue.category,
                "backup_available": issue.backup_available,
            })
        })
        .collect::<Vec<_>>();
    json!({ "ready": status.ready, "issues": issues })
}

fn commit_response(id: u64, draft_id: u64, commit: ConfigCommit) -> Value {
    success_response(
        id,
        json!({
            "draft_id": draft_id,
            "scope": commit.scope,
            "base_revision": commit.base_revision.to_string(),
            "revision": commit.revision.to_string(),
            "changes": commit.changes,
            "written": commit.written,
        }),
    )
}

fn invalid_params(id: u64) -> Value {
    error_response(
        Some(id),
        "invalid_request",
        "request parameters are invalid",
        None,
    )
}

fn unknown_draft(id: u64) -> Value {
    error_response(
        Some(id),
        "unknown_draft",
        "draft is not active on this connection",
        None,
    )
}

fn config_error_response(id: u64, error: &ConfigRuntimeError) -> Value {
    let path = match error {
        ConfigRuntimeError::UnknownObject(path)
        | ConfigRuntimeError::SecretReadForbidden(path)
        | ConfigRuntimeError::WrongType { path, .. }
        | ConfigRuntimeError::InvalidValue { path, .. } => Some(path.as_str()),
        _ => None,
    };
    let message = match error {
        ConfigRuntimeError::InvalidValue { reason, .. } => reason.as_str(),
        _ => match error.category() {
            ConfigErrorCategory::UnknownObject => "config object is unknown",
            ConfigErrorCategory::WrongType => "config value has the wrong type",
            ConfigErrorCategory::InvalidValue => "config value is invalid",
            ConfigErrorCategory::ReadOnlyScope => "config scope is read-only",
            ConfigErrorCategory::RevisionConflict => "draft base revision is stale",
            ConfigErrorCategory::SecretReadForbidden => "secret config values cannot be read",
            ConfigErrorCategory::RepairRequired => "config repair is required",
            ConfigErrorCategory::ResourceBusy => "config resource is busy",
            ConfigErrorCategory::Io => "config storage is unavailable",
        },
    };
    error_response(Some(id), error_category(error.category()), message, path)
}

fn credential_error_response(id: u64, error: CredentialVaultError) -> Value {
    let (category, message) = match error {
        CredentialVaultError::InvalidScope(_) => ("invalid_value", "credential scope is invalid"),
        CredentialVaultError::InvalidSecret => ("invalid_value", "credential secret is invalid"),
        CredentialVaultError::AlreadyBound => (
            "credential_already_bound",
            "credential reference is already bound",
        ),
        CredentialVaultError::NotFound => {
            ("credential_not_found", "credential reference was not found")
        }
        CredentialVaultError::Unavailable => (
            "credential_unavailable",
            "platform credential vault is unavailable",
        ),
    };
    error_response(Some(id), category, message, None)
}

fn error_category(category: ConfigErrorCategory) -> &'static str {
    match category {
        ConfigErrorCategory::UnknownObject => "unknown_object",
        ConfigErrorCategory::WrongType => "wrong_type",
        ConfigErrorCategory::InvalidValue => "invalid_value",
        ConfigErrorCategory::ReadOnlyScope => "read_only_scope",
        ConfigErrorCategory::RevisionConflict => "revision_conflict",
        ConfigErrorCategory::SecretReadForbidden => "secret_read_forbidden",
        ConfigErrorCategory::RepairRequired => "repair_required",
        ConfigErrorCategory::ResourceBusy => "resource_busy",
        ConfigErrorCategory::Io => "io",
    }
}

fn error_response(
    id: Option<u64>,
    category: &'static str,
    message: &str,
    path: Option<&str>,
) -> Value {
    let mut error = json!({
        "category": category,
        "message": message,
    });
    if let Some(path) = path {
        error["path"] = Value::String(path.to_owned());
    }
    json!({ "id": id, "error": error })
}

enum RequestLine {
    End,
    Value(Vec<u8>),
    TooLong,
}

fn read_request_line(reader: &mut impl BufRead) -> Result<RequestLine, io::Error> {
    let mut line = Vec::new();
    let mut too_long = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if line.is_empty() && !too_long {
                return Ok(RequestLine::End);
            }
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let payload_len = newline.unwrap_or(available.len());
        if !too_long {
            if line.len().saturating_add(payload_len) > MAX_REQUEST_BYTES {
                too_long = true;
                line.fill(0);
                line.clear();
            } else {
                line.extend_from_slice(&available[..payload_len]);
            }
        }
        let consumed = payload_len + usize::from(newline.is_some());
        reader.consume(consumed);
        if newline.is_some() {
            break;
        }
    }
    if too_long {
        return Ok(RequestLine::TooLong);
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(RequestLine::Value(line))
}

#[derive(Debug)]
pub(crate) enum AppServerError {
    Io(io::Error),
    Json(serde_json::Error),
}

impl fmt::Display for AppServerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("App Server I/O failed"),
            Self::Json(_) => formatter.write_str("App Server response encoding failed"),
        }
    }
}

impl Error for AppServerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Json(source) => Some(source),
        }
    }
}

impl From<io::Error> for AppServerError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<serde_json::Error> for AppServerError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(source)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self, Cursor, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    use greentyper_core::agent_team::{
        Capability, CapabilitySnapshot, CommandOutcome, InheritedModelPreset, ResourceBudget,
        TaskScope, TaskSpec, TeamCommand, TeamOperationRecord,
    };
    use greentyper_core::config::{ConfigDocument, ConfigPaths, ConfigRuntime};
    use greentyper_core::ledger::LedgerHead;
    use greentyper_core::model::TurnId;
    use greentyper_core::provider::ProviderDialect;
    use greentyper_core::runtime::{
        ProviderToolApproval, RecoveryStatus, RuntimeKernel, RuntimeSnapshot,
    };
    use greentyper_core::tool_runtime::{AuthorizedToolCall, ToolEffectExecutor, ToolExecution};
    use serde_json::{Value, json};

    use super::{AppServer, BoxedToolExecutor, run_stdio_with_vault, runtime_status};
    use crate::credential_vault::{
        CredentialVault, InMemoryCredentialVault, ProviderCredentialScope, SecretValue,
    };
    use crate::product_driver::{ProductDriver, ProductDriverError, ProductInteraction};
    use crate::provider_http::ConfiguredProvider;

    const TOOL_TEST_SECRET: &[u8] = b"private-app-server-tool-secret";
    const TOOL_CALL_SSE: &[u8] =
        include_bytes!("../../../tests/fixtures/provider/responses/v1/http-tool-call.sse");
    const TOOL_CONTINUATION_SSE: &[u8] =
        include_bytes!("../../../tests/fixtures/provider/responses/v1/http-tool-continuation.sse");
    const TEXT_SSE: &[u8] =
        include_bytes!("../../../tests/fixtures/provider/responses/v1/http-text.sse");

    #[test]
    fn runtime_status_exposes_provider_retry_eligibility() {
        let turn = TurnId::new(7).expect("Turn ID");
        let status = runtime_status(RuntimeSnapshot {
            head: LedgerHead::default(),
            thread: None,
            items: Vec::new(),
            status: RecoveryStatus::Blocked {
                turn,
                reason: "Provider stream failed before its first event".into(),
                retryable: true,
            },
            pending_agent: None,
            pending_model_selection: None,
            recovered_tail_bytes: 0,
        });
        assert_eq!(status["status"], "blocked");
        assert_eq!(status["turn"], turn.get());
        assert_eq!(status["retryable"], true);
    }

    struct InterruptingInteraction;

    impl ProductInteraction for InterruptingInteraction {
        fn present_team_operation(&mut self, _record: TeamOperationRecord) -> io::Result<()> {
            Ok(())
        }

        fn decide_tool(
            &mut self,
            _approval: &ProviderToolApproval,
        ) -> io::Result<super::ProductToolDecision> {
            Err(io::Error::other("interrupt before Tool decision"))
        }
    }

    struct NeverExecutor;

    impl ToolEffectExecutor for NeverExecutor {
        fn execute(&mut self, _call: &AuthorizedToolCall<'_>) -> ToolExecution {
            panic!("interrupted approval must not execute the Tool")
        }
    }

    struct CountingEchoExecutor {
        calls: Arc<AtomicUsize>,
    }

    impl ToolEffectExecutor for CountingEchoExecutor {
        fn execute(&mut self, call: &AuthorizedToolCall<'_>) -> ToolExecution {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(call.tool(), "local.echo");
            assert_eq!(
                call.arguments().canonical_json(),
                r#"{"message":"tool says hi"}"#
            );
            ToolExecution::Succeeded {
                output: b"tool says hi".to_vec(),
            }
        }
    }

    struct ToolDecisionFixture {
        root: PathBuf,
        runtime_path: PathBuf,
        config: ConfigRuntime,
        vault: InMemoryCredentialVault,
        server: JoinHandle<()>,
    }

    fn tool_decision_fixture(name: &str, responses: Vec<&'static [u8]>) -> ToolDecisionFixture {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "greentyper-app-server-tool-{name}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create App Server Tool test directory");
        let runtime_path = root.join("runtime.ledger");
        let (base_url, server) = spawn_tool_server(responses);
        let document = ConfigDocument::parse(&format!(
            r#"
schema_version = 1

[provider]
profile = "app-server-tool"
model = "fixture-model"

[providers.app-server-tool]
template = "openai-compatible"
credential = "app-server-tool"
base_url = {base_url:?}
dialects = ["responses"]
allow_insecure_loopback = true

[providers.app-server-tool.routes]
responses = "/v1/responses"

[providers.app-server-tool.pricing]
source = "unknown"
"#,
        ))
        .expect("parse App Server Tool Config");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        let config = ConfigRuntime::open(paths, document).expect("open App Server Tool Config");
        let profile = config
            .selected_provider_profile()
            .expect("resolve App Server Tool profile")
            .expect("external App Server Tool profile");
        let scope = ProviderCredentialScope::from_profile(&profile)
            .expect("App Server Tool credential scope");
        let mut initial_vault = InMemoryCredentialVault::default();
        initial_vault
            .bind(
                &scope,
                SecretValue::new(TOOL_TEST_SECRET.to_vec()).expect("Tool test secret"),
            )
            .expect("bind initial Tool credential");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(
                &scope,
                SecretValue::new(TOOL_TEST_SECRET.to_vec()).expect("Tool test secret"),
            )
            .expect("bind App Server Tool credential");
        let mut provider = ConfiguredProvider::for_new_turn_with_dialect(
            profile,
            "fixture-model",
            ProviderDialect::Responses,
            initial_vault,
        )
        .expect("construct initial App Server Tool Provider");
        provider.enable_local_echo();
        let layers = config
            .config_layers()
            .expect("App Server Tool Config layers")
            .clone();
        let mut interaction = InterruptingInteraction;
        let mut driver =
            ProductDriver::open_with_executor(&runtime_path, NeverExecutor, &mut interaction)
                .expect("open interrupted App Server Product driver");
        assert!(matches!(
            driver.execute(
                &layers,
                "echo through App Server",
                &mut provider,
                &mut interaction,
            ),
            Err(ProductDriverError::Interaction(_))
        ));
        drop(driver);
        ToolDecisionFixture {
            root,
            runtime_path,
            config,
            vault,
            server,
        }
    }

    fn spawn_tool_server(responses: Vec<&'static [u8]>) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind App Server Tool server");
        let address = listener
            .local_addr()
            .expect("App Server Tool server address");
        let handle = thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().expect("accept App Server Tool request");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .expect("set App Server Tool read timeout");
                let request = read_http_request(&mut stream);
                assert!(request.starts_with("POST /v1/responses HTTP/1.1\r\n"));
                assert!(request.contains("Bearer private-app-server-tool-secret"));
                write_http_response(&mut stream, response);
            }
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn app_server_runs_a_turn_for_the_exact_active_child_agent() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "greentyper-app-server-child-turn-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create child Turn directory");
        let runtime_path = root.join("runtime.ledger");
        let team_path = runtime_path.with_extension("ledger.team");
        let tool_path = runtime_path.with_extension("ledger.tool");
        let (base_url, server) = spawn_tool_server(vec![TEXT_SSE]);
        let document = ConfigDocument::parse(&format!(
            r#"
schema_version = 2

[agent]
default_model_preset = "child-default"

[providers.child-provider]
template = "openai-compatible"
credential = "child-secret"
base_url = {base_url:?}
dialects = ["responses"]
allow_insecure_loopback = true

[providers.child-provider.routes]
responses = "/v1/responses"

[providers.child-provider.pricing]
source = "unknown"

[model_presets.child-default]
provider = "child-provider"
model = "fixture-model"
dialect = "responses"
"#,
        ))
        .expect("parse child Turn Config");
        let config = ConfigRuntime::open(
            ConfigPaths::new(root.join("user.toml"), root.join("project.toml")),
            document,
        )
        .expect("open child Turn Config");
        let profile = config
            .provider_profile("child-provider")
            .expect("resolve child profile")
            .expect("external child profile");
        let scope = ProviderCredentialScope::from_profile(&profile)
            .expect("child Provider credential scope");
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(
                &scope,
                SecretValue::new(TOOL_TEST_SECRET.to_vec()).expect("child Provider secret"),
            )
            .expect("bind child Provider credential");

        let (mut kernel, recovery) =
            RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 2)
                .expect("open child Product state");
        assert!(recovery.into_sessions().is_empty());
        let root_admission = kernel
            .dispatch_team(TeamCommand::AdmitRoot {
                task: TaskSpec::new("root", TaskScope::default()),
                budget: ResourceBudget::new(2_000, 2),
                capabilities: CapabilitySnapshot::from_capabilities([
                    Capability::Process,
                    Capability::Tool("local.echo".into()),
                ]),
            })
            .expect("admit root");
        kernel
            .acknowledge_team_operation(root_admission.operation)
            .expect("acknowledge root");
        let root_session = match root_admission.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            other => panic!("unexpected root admission: {other:?}"),
        };
        let delegation = kernel
            .dispatch_team(TeamCommand::DelegateWithModelPreset {
                parent: root_session,
                task: TaskSpec::new("child", TaskScope::default()),
                budget: ResourceBudget::new(500, 1),
                capabilities: CapabilitySnapshot::default(),
                inherited_model_preset: Some(
                    InheritedModelPreset::new("child-default").expect("inherited Preset"),
                ),
            })
            .expect("delegate child");
        kernel
            .acknowledge_team_operation(delegation.operation)
            .expect("acknowledge child");
        let child = match delegation.commit.outcome {
            CommandOutcome::Delegated { agent, .. } => agent,
            other => panic!("unexpected delegation: {other:?}"),
        };
        drop(kernel);
        let team_before = fs::read(&team_path).expect("read Team before child Turn");
        let tool_before = fs::read(&tool_path).expect("read Tool before child Turn");

        let mut app = AppServer::new(config, runtime_path.clone(), &mut vault, || {
            Ok(BoxedToolExecutor(Box::new(NeverExecutor)))
        });
        let prepared = app.handle(
            json!({
                "id": 1,
                "operation": "agent.turn",
                "params": {"agent": child.get(), "input": "private child input"},
            })
            .to_string()
            .as_bytes(),
        );
        let config = app.config.reload_candidate().expect("reopen child Config");
        drop(app);
        server.join().expect("join child Provider server");

        assert_eq!(prepared["result"]["status"], "prepared");
        assert_eq!(prepared["result"]["text"], "fixture network 中");
        assert_eq!(prepared["result"]["usage_record_count"], 1);
        let delivery = prepared["result"]["delivery"]
            .as_u64()
            .expect("child delivery ID");
        let mut reopened = AppServer::new(config, runtime_path.clone(), &mut vault, || {
            Ok(BoxedToolExecutor(Box::new(NeverExecutor)))
        });
        let recovered = reopened.handle(
            format!(
                "{{\"id\":2,\"operation\":\"runtime.delivery\",\"params\":{{\"delivery\":{delivery}}}}}"
            )
            .as_bytes(),
        );
        let acknowledged = reopened.handle(
            format!(
                "{{\"id\":3,\"operation\":\"runtime.acknowledge\",\"params\":{{\"delivery\":{delivery}}}}}"
            )
            .as_bytes(),
        );
        drop(reopened);
        assert_eq!(recovered["result"]["status"], "prepared");
        assert_eq!(recovered["result"]["text"], "fixture network 中");
        assert_eq!(recovered["result"]["delivery"], delivery);
        assert_eq!(acknowledged["result"]["status"], "acknowledged");
        let usage = RuntimeKernel::inspect_usage(
            &runtime_path,
            greentyper_core::usage::UsageTimestamp::now().expect("usage time"),
        )
        .expect("inspect child usage");
        assert_eq!(usage.attempts().len(), 1);
        assert_eq!(usage.attempts()[0].agent(), Some(child.get()));
        assert_eq!(usage.attempts()[0].provider_profile(), "child-provider");
        assert_eq!(usage.attempts()[0].requested_model(), "fixture-model");
        assert_eq!(
            fs::read(&team_path).expect("read Team after child Turn"),
            team_before
        );
        assert_eq!(
            fs::read(&tool_path).expect("read Tool after child Turn"),
            tool_before
        );
        assert!(!root.join("user.toml").exists());
        assert!(!root.join("project.toml").exists());
        let output = prepared.to_string();
        assert!(!output.contains("private child input"));
        assert!(!output.contains(std::str::from_utf8(TOOL_TEST_SECRET).unwrap()));
        fs::remove_dir_all(root).expect("remove child Turn directory");
    }

    fn read_http_request(stream: &mut TcpStream) -> String {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let target_len = loop {
            let read = stream
                .read(&mut buffer)
                .expect("read App Server Tool request");
            assert!(read > 0, "App Server Tool request ended before headers");
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let headers =
                std::str::from_utf8(&request[..header_end]).expect("UTF-8 App Server Tool headers");
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .expect("App Server Tool Content-Length");
            break header_end + 4 + content_length;
        };
        while request.len() < target_len {
            let read = stream.read(&mut buffer).expect("read App Server Tool body");
            assert!(read > 0, "App Server Tool request ended before body");
            request.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(request).expect("UTF-8 App Server Tool request")
    }

    fn write_http_response(stream: &mut TcpStream, body: &[u8]) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("write App Server Tool response headers");
        stream
            .write_all(body)
            .expect("write App Server Tool response body");
        stream.flush().expect("flush App Server Tool response");
    }

    #[test]
    fn app_server_binds_and_replaces_credentials_without_readback() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "greentyper-app-server-credential-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create App Server credential test directory");
        let user = root.join("user.toml");
        let project = root.join("project.toml");
        let config =
            ConfigRuntime::open(ConfigPaths::new(&user, &project), ConfigDocument::empty())
                .expect("open Config Runtime");
        let first_secret = "private-app-server-first";
        let second_secret = "private-app-server-second";
        let requests = format!(
            concat!(
                "{{\"id\":1,\"operation\":\"credential.bind\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://api.example.com/v1\",\"secret\":\"{first_secret}\"}}}}\n",
                "{{\"id\":2,\"operation\":\"credential.bind\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://api.example.com/v1\",\"secret\":\"{second_secret}\"}}}}\n",
                "{{\"id\":3,\"operation\":\"credential.replace\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://other.example.com/v1\",\"secret\":\"{second_secret}\"}}}}\n",
                "{{\"id\":4,\"operation\":\"credential.replace\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://api.example.com/v1\",\"secret\":\"{second_secret}\"}}}}\n",
            ),
            first_secret = first_secret,
            second_secret = second_secret,
        );
        let mut output = Vec::new();
        let mut vault = InMemoryCredentialVault::default();

        run_stdio_with_vault(
            Cursor::new(requests.as_bytes()),
            &mut output,
            config,
            root.join("runtime.ledger"),
            &mut vault,
        )
        .expect("run App Server credential flow");

        let responses = String::from_utf8(output).expect("UTF-8 App Server output");
        let responses = responses
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSON response"))
            .collect::<Vec<_>>();
        assert_eq!(responses[0]["result"]["status"], "bound");
        assert_eq!(
            responses[1]["error"]["category"],
            "credential_already_bound"
        );
        assert_eq!(responses[2]["error"]["category"], "credential_not_found");
        assert_eq!(responses[3]["result"]["status"], "replaced");
        let output = responses.iter().map(Value::to_string).collect::<String>();
        assert!(!output.contains(first_secret));
        assert!(!output.contains(second_secret));

        let scope = ProviderCredentialScope::new(
            "openai-main",
            "openai-main",
            "https://api.example.com/v1",
            false,
        )
        .expect("credential scope");
        assert_eq!(
            vault.resolve(&scope).expect("stored credential").expose(),
            second_secret.as_bytes()
        );
        assert!(!user.exists());
        assert!(!project.exists());
        fs::remove_dir_all(root).expect("remove App Server credential test directory");
    }

    #[test]
    fn app_server_tests_and_forgets_only_the_origin_bound_credential() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "greentyper-app-server-credential-status-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create App Server credential status directory");
        let user = root.join("user.toml");
        let project = root.join("project.toml");
        let config =
            ConfigRuntime::open(ConfigPaths::new(&user, &project), ConfigDocument::empty())
                .expect("open Config Runtime");
        let secret = "private-app-server-status";
        let requests = format!(
            concat!(
                "{{\"id\":1,\"operation\":\"credential.test\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://api.example.com/v1\"}}}}\n",
                "{{\"id\":2,\"operation\":\"credential.bind\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://api.example.com/v1\",\"secret\":\"{secret}\"}}}}\n",
                "{{\"id\":3,\"operation\":\"credential.test\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://api.example.com/v1\"}}}}\n",
                "{{\"id\":4,\"operation\":\"credential.test\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://other.example.com/v1\"}}}}\n",
                "{{\"id\":5,\"operation\":\"credential.forget\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://other.example.com/v1\"}}}}\n",
                "{{\"id\":6,\"operation\":\"credential.forget\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://api.example.com/v1\"}}}}\n",
                "{{\"id\":7,\"operation\":\"credential.test\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://api.example.com/v1\"}}}}\n",
                "{{\"id\":8,\"operation\":\"credential.forget\",\"params\":{{\"reference\":\"openai-main\",\"profile\":\"openai-main\",\"origin\":\"https://api.example.com/v1\"}}}}\n",
            ),
            secret = secret,
        );
        let mut output = Vec::new();
        let mut vault = InMemoryCredentialVault::default();

        run_stdio_with_vault(
            Cursor::new(requests.as_bytes()),
            &mut output,
            config,
            root.join("runtime.ledger"),
            &mut vault,
        )
        .expect("run App Server credential status flow");

        let output = String::from_utf8(output).expect("UTF-8 App Server output");
        assert!(!output.contains(secret));
        let responses = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSON response"))
            .collect::<Vec<_>>();
        assert_eq!(responses[0]["result"]["status"], "not_found");
        assert_eq!(responses[1]["result"]["status"], "bound");
        assert_eq!(responses[2]["result"]["status"], "available");
        assert_eq!(responses[3]["result"]["status"], "not_found");
        assert_eq!(responses[4]["result"]["status"], "not_found");
        assert_eq!(responses[5]["result"]["status"], "forgotten");
        assert_eq!(responses[6]["result"]["status"], "not_found");
        assert_eq!(responses[7]["result"]["status"], "not_found");

        let scope = ProviderCredentialScope::new(
            "openai-main",
            "openai-main",
            "https://api.example.com/v1",
            false,
        )
        .expect("credential scope");
        assert!(vault.resolve(&scope).is_err());
        assert!(!user.exists());
        assert!(!project.exists());
        fs::remove_dir_all(root).expect("remove App Server credential status directory");
    }

    #[test]
    fn app_server_rejects_invalid_credential_input_and_keeps_the_stream_usable() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "greentyper-app-server-credential-boundary-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create App Server credential boundary directory");
        let user = root.join("user.toml");
        let project = root.join("project.toml");
        let config =
            ConfigRuntime::open(ConfigPaths::new(&user, &project), ConfigDocument::empty())
                .expect("open Config Runtime");
        let overlong_secret = "x".repeat(crate::credential_vault::MAX_SECRET_BYTES + 1);
        let valid_secret = "private-valid-after-errors";
        let mut requests = [
            json!({"id": 1, "operation": "credential.bind", "params": {
                "reference": "openai-main", "profile": "openai-main",
                "origin": "https://api.example.com/v1", "secret": ""
            }}),
            json!({"id": 2, "operation": "credential.bind", "params": {
                "reference": "openai-main", "profile": "openai-main",
                "origin": "https://api.example.com/v1", "secret": "private\ncontrol"
            }}),
            json!({"id": 3, "operation": "credential.bind", "params": {
                "reference": "openai-main", "profile": "openai-main",
                "origin": "https://api.example.com/v1", "secret": overlong_secret
            }}),
            json!({"id": 4, "operation": "credential.bind", "params": {
                "reference": "openai-main", "profile": "openai-main",
                "origin": "http://api.example.com/v1", "secret": "private-invalid-origin"
            }}),
            json!({"id": 5, "operation": "credential.bind", "params": {
                "reference": "openai-main", "profile": "openai-main",
                "origin": "https://api.example.com/v1", "secret": "private-extra-field",
                "extra": true
            }}),
            json!({"id": 6, "operation": "credential.bind", "params": {
                "reference": "openai-main", "profile": "openai-main",
                "origin": "https://api.example.com/v1", "secret": valid_secret
            }}),
            json!({"id": 7, "operation": "credential.test", "params": {
                "reference": "openai-main", "profile": "openai-main",
                "origin": "https://api.example.com/v1"
            }}),
        ]
        .into_iter()
        .map(|request| request.to_string())
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        requests.push_str(
            "{\"id\":8,\"operation\":\"credential.bind\",\"params\":{\"reference\":\"duplicate\",\"profile\":\"openai-main\",\"origin\":\"https://api.example.com/v1\",\"secret\":\"private-duplicate-first\",\"secret\":\"private-duplicate-second\"}}\n",
        );
        for request in [
            json!({"id": 9, "operation": "credential.bind", "params": {
                "reference": "loopback", "profile": "openai-main",
                "origin": "http://127.0.0.1:8080/v1", "secret": "private-loopback-denied"
            }}),
            json!({"id": 10, "operation": "credential.bind", "params": {
                "reference": "loopback", "profile": "openai-main",
                "origin": "http://127.0.0.1:8080/v1", "allow_insecure_loopback": true,
                "secret": "private-loopback-allowed"
            }}),
            json!({"id": 11, "operation": "credential.test", "params": {
                "reference": "loopback", "profile": "openai-main",
                "origin": "http://127.0.0.1:8080/v1", "allow_insecure_loopback": true
            }}),
            json!({"id": 12, "operation": "credential.forget", "params": {
                "reference": "loopback", "profile": "openai-main",
                "origin": "http://127.0.0.1:8080/v1", "allow_insecure_loopback": true
            }}),
        ] {
            requests.push_str(&request.to_string());
            requests.push('\n');
        }
        let mut output = Vec::new();
        let mut vault = InMemoryCredentialVault::default();

        run_stdio_with_vault(
            Cursor::new(requests.as_bytes()),
            &mut output,
            config,
            root.join("runtime.ledger"),
            &mut vault,
        )
        .expect("run App Server credential boundary flow");

        let output = String::from_utf8(output).expect("UTF-8 App Server output");
        for secret in [
            "private\ncontrol",
            "private-invalid-origin",
            "private-extra-field",
            "private-duplicate-first",
            "private-duplicate-second",
            "private-loopback-denied",
            "private-loopback-allowed",
            valid_secret,
        ] {
            assert!(!output.contains(secret));
        }
        let responses = output
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("JSON response"))
            .collect::<Vec<_>>();
        for response in &responses[..4] {
            assert_eq!(response["error"]["category"], "invalid_value");
        }
        assert_eq!(responses[4]["error"]["category"], "invalid_request");
        assert_eq!(responses[5]["result"]["status"], "bound");
        assert_eq!(responses[6]["result"]["status"], "available");
        assert_eq!(responses[7]["error"]["category"], "invalid_request");
        assert_eq!(responses[8]["error"]["category"], "invalid_value");
        assert_eq!(responses[9]["result"]["status"], "bound");
        assert_eq!(responses[10]["result"]["status"], "available");
        assert_eq!(responses[11]["result"]["status"], "forgotten");
        assert!(!user.exists());
        assert!(!project.exists());
        fs::remove_dir_all(root).expect("remove App Server credential boundary directory");
    }

    #[test]
    fn app_server_tool_review_never_admits_a_missing_root_session() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "greentyper-app-server-empty-tool-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create empty Tool review directory");
        let runtime_path = root.join("runtime.ledger");
        let team_path = runtime_path.with_extension("ledger.team");
        let tool_path = runtime_path.with_extension("ledger.tool");
        let (kernel, recovery) =
            RuntimeKernel::open_with_team_and_tools(&runtime_path, &team_path, &tool_path, 1)
                .expect("create empty Product Ledgers");
        assert!(recovery.into_sessions().is_empty());
        drop(kernel);
        let runtime_before = fs::read(&runtime_path).expect("read empty Runtime Ledger");
        let team_before = fs::read(&team_path).expect("read empty Team Ledger");
        let tool_before = fs::read(&tool_path).expect("read empty Tool Ledger");
        let user = root.join("user.toml");
        let project = root.join("project.toml");
        let config =
            ConfigRuntime::open(ConfigPaths::new(&user, &project), ConfigDocument::empty())
                .expect("open empty Tool review Config");
        let mut vault = InMemoryCredentialVault::default();
        let mut app = AppServer::new(config, runtime_path.clone(), &mut vault, || {
            panic!("empty Team review must not construct an executor")
        });

        let response = app.handle(
            br#"{"id":1,"operation":"tool.decide","params":{"call":1,"decision":"review"}}"#,
        );

        assert_eq!(response["error"]["category"], "tool_unavailable");
        assert_eq!(
            fs::read(&runtime_path).expect("read Runtime after review"),
            runtime_before
        );
        assert_eq!(
            fs::read(&team_path).expect("read Team after review"),
            team_before
        );
        assert_eq!(
            fs::read(&tool_path).expect("read Tool after review"),
            tool_before
        );
        assert!(!user.exists());
        assert!(!project.exists());
        fs::remove_dir_all(root).expect("remove empty Tool review directory");
    }

    #[test]
    fn app_server_approves_or_denies_a_recovered_tool_with_exact_authority() {
        let approved = tool_decision_fixture(
            "approve",
            vec![
                TOOL_CALL_SSE,
                TOOL_CALL_SSE,
                TOOL_CALL_SSE,
                TOOL_CONTINUATION_SSE,
            ],
        );
        let ToolDecisionFixture {
            root,
            runtime_path,
            config,
            mut vault,
            server,
        } = approved;
        let team_path = runtime_path.with_extension("ledger.team");
        let team_before = fs::read(&team_path).expect("read Team before App Server approval");
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::clone(&calls);
        let mut app = AppServer::new(config, runtime_path.clone(), &mut vault, move || {
            Ok(BoxedToolExecutor(Box::new(CountingEchoExecutor {
                calls: Arc::clone(&factory_calls),
            })))
        });
        let zero_hash = "00".repeat(32);
        let missing_review = app.handle(
            json!({
                "id": 1,
                "operation": "tool.decide",
                "params": {
                    "call": 1,
                    "decision": "approve",
                    "arguments_sha256": zero_hash,
                    "resources_sha256": zero_hash,
                },
            })
            .to_string()
            .as_bytes(),
        );
        assert_eq!(missing_review["error"]["category"], "tool_review_required");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let review = app.handle(
            br#"{"id":2,"operation":"tool.decide","params":{"call":1,"decision":"review"}}"#,
        );
        assert_eq!(review["result"]["status"], "review_required");
        assert_eq!(review["result"]["arguments"]["message"], "tool says hi");
        assert_eq!(
            review["result"]["resources"]["process"],
            "greentyper.local.echo.v1"
        );
        let arguments_sha256 = review["result"]["confirmation"]["arguments_sha256"]
            .as_str()
            .expect("reviewed arguments hash");
        let resources_sha256 = review["result"]["confirmation"]["resources_sha256"]
            .as_str()
            .expect("reviewed resources hash");
        assert_eq!(arguments_sha256.len(), 64);
        assert_eq!(resources_sha256.len(), 64);
        let wrong_review = app.handle(
            json!({
                "id": 3,
                "operation": "tool.decide",
                "params": {
                    "call": 1,
                    "decision": "approve",
                    "arguments_sha256": arguments_sha256,
                    "resources_sha256": "00".repeat(32),
                },
            })
            .to_string()
            .as_bytes(),
        );
        assert_eq!(wrong_review["error"]["category"], "tool_approval_mismatch");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let approved = app.handle(
            json!({
                "id": 4,
                "operation": "tool.decide",
                "params": {
                    "call": 1,
                    "decision": "approve",
                    "arguments_sha256": arguments_sha256,
                    "resources_sha256": resources_sha256,
                },
            })
            .to_string()
            .as_bytes(),
        );
        let delivery =
            app.handle(br#"{"id":5,"operation":"runtime.delivery","params":{"delivery":1}}"#);
        let status = app.handle(br#"{"id":6,"operation":"tool.status"}"#);
        let acknowledged =
            app.handle(br#"{"id":7,"operation":"runtime.acknowledge","params":{"delivery":1}}"#);
        let runtime_status = app.handle(br#"{"id":8,"operation":"runtime.status"}"#);
        let output_text = [
            &missing_review,
            &review,
            &wrong_review,
            &approved,
            &delivery,
            &status,
            &acknowledged,
            &runtime_status,
        ]
        .into_iter()
        .map(Value::to_string)
        .collect::<String>();
        server.join().expect("join App Server approval server");
        assert!(!output_text.contains(std::str::from_utf8(TOOL_TEST_SECRET).unwrap()));
        assert!(!output_text.contains("call_http_echo_1"));
        assert_eq!(approved["result"]["status"], "prepared");
        assert_eq!(approved["result"]["call"], 1);
        assert_eq!(approved["result"]["delivery"], 1);
        assert_eq!(approved["result"]["text"], "Echoed: tool says hi");
        assert_eq!(approved["result"]["usage_record_count"], 2);
        assert_eq!(delivery["result"]["text"], "Echoed: tool says hi");
        assert_eq!(status["result"]["calls"][0]["status"], "succeeded");
        assert_eq!(acknowledged["result"]["status"], "acknowledged");
        assert_eq!(runtime_status["result"]["status"], "ready");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            fs::read(&team_path).expect("read Team after App Server approval"),
            team_before
        );
        assert!(!root.join("user.toml").exists());
        assert!(!root.join("project.toml").exists());
        fs::remove_dir_all(&root).expect("remove App Server approval directory");

        let denied =
            tool_decision_fixture("deny", vec![TOOL_CALL_SSE, TOOL_CALL_SSE, TOOL_CALL_SSE]);
        let ToolDecisionFixture {
            root,
            runtime_path,
            config,
            mut vault,
            server,
        } = denied;
        let team_path = runtime_path.with_extension("ledger.team");
        let tool_path = runtime_path.with_extension("ledger.tool");
        let team_before = fs::read(&team_path).expect("read Team before App Server denial");
        let tool_before = fs::read(&tool_path).expect("read Tool before App Server denial");
        let calls = Arc::new(AtomicUsize::new(0));
        let factory_calls = Arc::clone(&calls);
        let mut app = AppServer::new(config, runtime_path.clone(), &mut vault, move || {
            Ok(BoxedToolExecutor(Box::new(CountingEchoExecutor {
                calls: Arc::clone(&factory_calls),
            })))
        });
        let review = app.handle(
            br#"{"id":9,"operation":"tool.decide","params":{"call":1,"decision":"review"}}"#,
        );
        let arguments_sha256 = review["result"]["confirmation"]["arguments_sha256"]
            .as_str()
            .expect("reviewed denial arguments hash");
        let resources_sha256 = review["result"]["confirmation"]["resources_sha256"]
            .as_str()
            .expect("reviewed denial resources hash");
        let denied = app.handle(
            json!({
                "id": 10,
                "operation": "tool.decide",
                "params": {
                    "call": 1,
                    "decision": "deny",
                    "arguments_sha256": arguments_sha256,
                    "resources_sha256": resources_sha256,
                },
            })
            .to_string()
            .as_bytes(),
        );
        let repeated = app.handle(
            json!({
                "id": 11,
                "operation": "tool.decide",
                "params": {
                    "call": 1,
                    "decision": "deny",
                    "arguments_sha256": arguments_sha256,
                    "resources_sha256": resources_sha256,
                },
            })
            .to_string()
            .as_bytes(),
        );
        let status = app.handle(br#"{"id":12,"operation":"tool.status"}"#);
        let runtime_status = app.handle(br#"{"id":13,"operation":"runtime.status"}"#);
        server.join().expect("join App Server denial server");
        let output_text = [&review, &denied, &repeated, &status, &runtime_status]
            .into_iter()
            .map(Value::to_string)
            .collect::<String>();
        assert!(!output_text.contains(std::str::from_utf8(TOOL_TEST_SECRET).unwrap()));
        assert!(!output_text.contains("call_http_echo_1"));
        assert_eq!(review["result"]["status"], "review_required");
        assert_eq!(denied["result"]["status"], "denied");
        assert_eq!(repeated["error"]["category"], "tool_review_required");
        assert_eq!(status["result"]["calls"][0]["status"], "denied");
        assert_eq!(runtime_status["result"]["status"], "blocked");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            fs::read(&team_path).expect("read Team after App Server denial"),
            team_before
        );
        assert_ne!(
            fs::read(&tool_path).expect("read denied Tool Ledger"),
            tool_before
        );
        assert!(!root.join("user.toml").exists());
        assert!(!root.join("project.toml").exists());
        fs::remove_dir_all(root).expect("remove App Server denial directory");
    }
}
