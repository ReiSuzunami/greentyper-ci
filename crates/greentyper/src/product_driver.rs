//! Product-owned Provider and Tool orchestration.

use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use greentyper_core::agent_team::{
    AgentSession, Capability, CapabilitySnapshot, CommandOutcome, ResourceBudget, TaskScope,
    TaskSpec, TeamCommand, TeamOperationRecord, TeamOperationStatus,
};
use greentyper_core::config::ConfigLayers;
use greentyper_core::model::DeliveryId;
use greentyper_core::pricing::PriceScheduleBook;
use greentyper_core::provider::{ProviderEpoch, ProviderRuntime};
#[cfg(test)]
use greentyper_core::runtime::RuntimeSnapshot;
use greentyper_core::runtime::{
    AcknowledgeOutcome, PreparedOutput, ProviderToolApproval, ProviderTurnOutcome, RuntimeError,
    RuntimeKernel,
};
use greentyper_core::tool_runtime::{ApprovalDecision, ToolEffectExecutor};
use greentyper_core::usage::UsageWindow;

use crate::local_process::{LOCAL_ECHO_TOOL, local_echo_resources};

const APPROVAL_LIFETIME: Duration = Duration::from_secs(5 * 60);
const DENIAL_REASON: &str = "user denied Provider Tool call";

pub(crate) trait ProductInteraction {
    fn present_team_operation(&mut self, record: TeamOperationRecord) -> io::Result<()>;

    fn decide_tool(&mut self, approval: &ProviderToolApproval) -> io::Result<ProductToolDecision>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProductToolDecision {
    Approve,
    Deny,
}

pub(crate) struct ProductDriver<E> {
    kernel: RuntimeKernel,
    session: AgentSession,
    executor: E,
}

impl<E: ToolEffectExecutor> ProductDriver<E> {
    pub(crate) fn open_with_executor(
        runtime_path: &Path,
        executor: E,
        interaction: &mut impl ProductInteraction,
    ) -> Result<Self, ProductDriverError> {
        has_product_driver_state(runtime_path)?;
        if let Some(parent) = runtime_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(ProductDriverError::Io)?;
        }
        let team_path = sidecar_path(runtime_path, "team");
        let tool_path = sidecar_path(runtime_path, "tool");
        let (mut kernel, recovery) =
            RuntimeKernel::open_with_team_and_tools(runtime_path, team_path, tool_path, 1)?;

        let pending_operations: Vec<_> = recovery
            .snapshot()
            .operations
            .iter()
            .filter(|record| record.status == TeamOperationStatus::CommittedAwaitingAcknowledgement)
            .copied()
            .collect();
        let mut sessions = recovery.into_sessions();
        for record in pending_operations {
            interaction
                .present_team_operation(record)
                .map_err(ProductDriverError::Interaction)?;
            kernel.acknowledge_team_operation(record.operation)?;
        }

        let session = match sessions.len() {
            0 => admit_root(&mut kernel, interaction)?,
            1 => sessions
                .pop()
                .ok_or(ProductDriverError::UnexpectedRecovery)?,
            _ => return Err(ProductDriverError::UnexpectedRecovery),
        };
        Ok(Self {
            kernel,
            session,
            executor,
        })
    }

    #[cfg(test)]
    pub(crate) fn execute(
        &mut self,
        layers: &ConfigLayers,
        input: impl Into<String>,
        provider: &mut impl ProviderRuntime,
        interaction: &mut impl ProductInteraction,
    ) -> Result<PreparedOutput, ProductDriverError> {
        self.execute_with_observability(
            layers,
            Vec::new(),
            PriceScheduleBook::default(),
            input,
            provider,
            interaction,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn execute_with_observability(
        &mut self,
        layers: &ConfigLayers,
        usage_windows: Vec<UsageWindow>,
        price_schedules: PriceScheduleBook,
        input: impl Into<String>,
        provider: &mut impl ProviderRuntime,
        interaction: &mut impl ProductInteraction,
    ) -> Result<PreparedOutput, ProductDriverError> {
        let outcome = self.kernel.execute_provider_turn_with_observability(
            self.session,
            layers,
            usage_windows,
            price_schedules,
            input,
            provider,
            local_echo_resources,
        )?;
        self.finish(outcome, provider, interaction)
    }

    pub(crate) fn resume(
        &mut self,
        provider: &mut impl ProviderRuntime,
        interaction: &mut impl ProductInteraction,
    ) -> Result<PreparedOutput, ProductDriverError> {
        let outcome =
            self.kernel
                .resume_provider_turn(self.session, provider, local_echo_resources)?;
        self.finish(outcome, provider, interaction)
    }

    pub(crate) fn acknowledge(
        &mut self,
        delivery: DeliveryId,
    ) -> Result<AcknowledgeOutcome, ProductDriverError> {
        self.kernel
            .acknowledge(delivery)
            .map_err(ProductDriverError::Runtime)
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> RuntimeSnapshot {
        self.kernel.snapshot()
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn team_snapshot(&self) -> Option<greentyper_core::runtime::KernelTeamSnapshot> {
        self.kernel.team_snapshot()
    }

    #[must_use]
    #[cfg(test)]
    pub(crate) fn tool_snapshot(&self) -> Option<greentyper_core::tool_runtime::ToolSnapshot> {
        self.kernel.tool_snapshot()
    }

    #[must_use]
    pub(crate) fn pending_provider_epoch(&self) -> Option<&ProviderEpoch> {
        self.kernel.pending_provider_epoch()
    }

    fn finish(
        &mut self,
        outcome: ProviderTurnOutcome,
        provider: &mut impl ProviderRuntime,
        interaction: &mut impl ProductInteraction,
    ) -> Result<PreparedOutput, ProductDriverError> {
        match outcome {
            ProviderTurnOutcome::Prepared(output) => Ok(output),
            ProviderTurnOutcome::ApprovalRequired(approval) => {
                let decision = interaction
                    .decide_tool(&approval)
                    .map_err(ProductDriverError::Interaction)?;
                let decision = match decision {
                    ProductToolDecision::Approve => ApprovalDecision::Grant {
                        expires_at_unix_ms: approval_expiry_unix_ms(),
                    },
                    ProductToolDecision::Deny => ApprovalDecision::Deny {
                        reason: DENIAL_REASON.into(),
                    },
                };
                self.kernel
                    .resolve_provider_tool_call(approval, decision, &mut self.executor, provider)
                    .map_err(ProductDriverError::Runtime)
            }
        }
    }
}

pub(crate) fn has_product_driver_state(runtime_path: &Path) -> Result<bool, ProductDriverError> {
    match (
        sidecar_path(runtime_path, "team").exists(),
        sidecar_path(runtime_path, "tool").exists(),
    ) {
        (false, false) => Ok(false),
        (true, true) => Ok(true),
        _ => Err(ProductDriverError::IncompleteState),
    }
}

fn admit_root(
    kernel: &mut RuntimeKernel,
    interaction: &mut impl ProductInteraction,
) -> Result<AgentSession, ProductDriverError> {
    let operation = kernel.dispatch_team(TeamCommand::AdmitRoot {
        task: TaskSpec::new(
            "drive one product Provider Turn",
            TaskScope::from_labels(["product-provider-turn"]),
        ),
        budget: ResourceBudget::new(1_000, 1),
        capabilities: CapabilitySnapshot::from_capabilities([
            Capability::Tool(LOCAL_ECHO_TOOL.into()),
            Capability::Process,
        ]),
    })?;
    let record = kernel
        .team_snapshot()
        .and_then(|snapshot| {
            snapshot
                .operations
                .into_iter()
                .find(|record| record.operation == operation.operation)
        })
        .ok_or(ProductDriverError::UnexpectedRecovery)?;
    interaction
        .present_team_operation(record)
        .map_err(ProductDriverError::Interaction)?;
    kernel.acknowledge_team_operation(operation.operation)?;
    match operation.commit.outcome {
        CommandOutcome::RootAdmitted { session, .. } => Ok(session),
        _ => Err(ProductDriverError::UnexpectedRecovery),
    }
}

fn approval_expiry_unix_ms() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let lifetime = APPROVAL_LIFETIME.as_millis();
    u64::try_from(now.saturating_add(lifetime)).unwrap_or(u64::MAX)
}

fn sidecar_path(runtime: &Path, kind: &str) -> PathBuf {
    let mut path = OsString::from(runtime.as_os_str());
    path.push(".");
    path.push(kind);
    PathBuf::from(path)
}

#[derive(Debug)]
pub(crate) enum ProductDriverError {
    Io(io::Error),
    Interaction(io::Error),
    Runtime(RuntimeError),
    IncompleteState,
    UnexpectedRecovery,
}

impl fmt::Display for ProductDriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(source) => write!(formatter, "Product driver I/O failed: {source}"),
            Self::Interaction(source) => write!(formatter, "Product interaction failed: {source}"),
            Self::Runtime(source) => write!(formatter, "{source}"),
            Self::IncompleteState => {
                write!(formatter, "Product driver sidecar state is incomplete")
            }
            Self::UnexpectedRecovery => {
                write!(formatter, "Product driver recovery is inconsistent")
            }
        }
    }
}

impl Error for ProductDriverError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) | Self::Interaction(source) => Some(source),
            Self::Runtime(source) => Some(source),
            Self::IncompleteState | Self::UnexpectedRecovery => None,
        }
    }
}

impl From<RuntimeError> for ProductDriverError {
    fn from(source: RuntimeError) -> Self {
        Self::Runtime(source)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::io;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use greentyper_core::agent_team::TeamOperationRecord;
    use greentyper_core::config::ConfigLayers;
    use greentyper_core::config::ConfigRuntimeStatus;
    use greentyper_core::provider::{
        ProviderError, ProviderEvent, ProviderRequest, ProviderRuntime, ProviderToolCall,
        ProviderToolOutput, UsageRecord,
    };
    use greentyper_core::runtime::{ProviderToolApproval, RecoveryStatus};
    use greentyper_core::tool_runtime::{AuthorizedToolCall, ToolEffectExecutor, ToolExecution};

    use super::*;
    use crate::presentation::{BlockerView, PresentationSources, TuiViewModel};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn approved_provider_tool_runs_once_and_finishes_the_turn() {
        let ledger = temp_path("approved");
        let calls = Rc::new(Cell::new(0));
        let mut interaction = RecordingInteraction::approve();
        let mut driver = ProductDriver::open_with_executor(
            &ledger,
            CountingEchoExecutor::new(Rc::clone(&calls)),
            &mut interaction,
        )
        .expect("open Product driver");
        let mut provider = LocalEchoProvider::default();

        let output = driver
            .execute(
                &ConfigLayers::default(),
                "echo with approval",
                &mut provider,
                &mut interaction,
            )
            .expect("complete approved Provider Tool Turn");

        assert_eq!(output.text(), "Echoed: approved message");
        assert_eq!(output.usage_records().len(), 2);
        assert_eq!(calls.get(), 1);
        assert_eq!(interaction.team_operations, 1);
        assert_eq!(interaction.approvals.len(), 1);
        assert_eq!(interaction.approvals[0].tool, "local.echo");
        assert_eq!(
            interaction.approvals[0].arguments,
            r#"{"message":"approved message"}"#
        );
        driver
            .acknowledge(output.delivery())
            .expect("acknowledge delivered output");
        assert_eq!(driver.snapshot().status, RecoveryStatus::Ready);

        drop(driver);
        cleanup(&ledger);
    }

    #[test]
    fn denied_provider_tool_never_reaches_the_executor() {
        let ledger = temp_path("denied");
        let calls = Rc::new(Cell::new(0));
        let mut interaction = RecordingInteraction::deny();
        let mut driver = ProductDriver::open_with_executor(
            &ledger,
            CountingEchoExecutor::new(Rc::clone(&calls)),
            &mut interaction,
        )
        .expect("open Product driver");
        let mut provider = LocalEchoProvider::default();

        let result = driver.execute(
            &ConfigLayers::default(),
            "deny this tool",
            &mut provider,
            &mut interaction,
        );

        assert!(matches!(result, Err(ProductDriverError::Runtime(_))));
        assert_eq!(calls.get(), 0);
        assert!(matches!(
            driver.snapshot().status,
            RecoveryStatus::Blocked { .. }
        ));
        drop(driver);
        cleanup(&ledger);
    }

    #[test]
    fn approval_interruption_reopens_and_executes_the_effect_once() {
        let ledger = temp_path("approval-recovery");
        let calls = Rc::new(Cell::new(0));
        let mut interrupted = RecordingInteraction::fail_approval();
        let mut driver = ProductDriver::open_with_executor(
            &ledger,
            CountingEchoExecutor::new(Rc::clone(&calls)),
            &mut interrupted,
        )
        .expect("open Product driver");
        let mut provider = LocalEchoProvider::default();
        let result = driver.execute(
            &ConfigLayers::default(),
            "recover approval",
            &mut provider,
            &mut interrupted,
        );
        assert!(matches!(result, Err(ProductDriverError::Interaction(_))));
        assert_eq!(calls.get(), 0);
        let runtime = driver.snapshot();
        let team = driver.team_snapshot().expect("Team snapshot");
        let tools = driver.tool_snapshot().expect("Tool snapshot");
        let config = ConfigRuntimeStatus {
            ready: true,
            issues: Vec::new(),
        };
        let view = TuiViewModel::build(
            "/",
            "",
            0,
            PresentationSources {
                runtime: &runtime,
                usage: None,
                team: Some(&team),
                tools: Some(&tools),
                config: &config,
                provider_profile: None,
                model: None,
                context_pressure: None,
                model_presets: &[],
                catalog_models: &[],
            },
        )
        .expect("approval presentation");
        assert!(view.blockers.iter().any(|blocker| matches!(
            blocker,
            BlockerView::ToolApproval {
                tool,
                expires_at_unix_ms: None,
                ..
            } if tool == "local.echo"
        )));
        drop(driver);

        let mut interaction = RecordingInteraction::approve();
        let mut recovered = ProductDriver::open_with_executor(
            &ledger,
            CountingEchoExecutor::new(Rc::clone(&calls)),
            &mut interaction,
        )
        .expect("reopen Product driver");
        let mut provider = LocalEchoProvider::default();
        let output = recovered
            .resume(&mut provider, &mut interaction)
            .expect("resume approval");
        assert_eq!(output.text(), "Echoed: approved message");
        assert_eq!(calls.get(), 1);
        assert_eq!(interaction.approvals.len(), 1);
        recovered
            .acknowledge(output.delivery())
            .expect("acknowledge resumed output");
        drop(recovered);
        cleanup(&ledger);
    }

    #[test]
    fn team_receipt_interruption_is_presented_again_before_acknowledgement() {
        let ledger = temp_path("team-receipt-recovery");
        let calls = Rc::new(Cell::new(0));
        let mut interrupted = RecordingInteraction::fail_team();
        let result = ProductDriver::open_with_executor(
            &ledger,
            CountingEchoExecutor::new(Rc::clone(&calls)),
            &mut interrupted,
        );
        assert!(matches!(result, Err(ProductDriverError::Interaction(_))));
        assert_eq!(interrupted.team_operations, 1);
        assert_eq!(calls.get(), 0);

        let mut interaction = RecordingInteraction::approve();
        let mut recovered = ProductDriver::open_with_executor(
            &ledger,
            CountingEchoExecutor::new(Rc::clone(&calls)),
            &mut interaction,
        )
        .expect("reopen Product driver after receipt interruption");
        assert_eq!(interaction.team_operations, 1);
        let mut provider = LocalEchoProvider::default();
        let output = recovered
            .execute(
                &ConfigLayers::default(),
                "continue after Team receipt",
                &mut provider,
                &mut interaction,
            )
            .expect("complete Turn after Team receipt recovery");
        assert_eq!(calls.get(), 1);
        recovered
            .acknowledge(output.delivery())
            .expect("acknowledge output");
        drop(recovered);
        cleanup(&ledger);
    }

    #[test]
    fn incomplete_sidecar_state_fails_closed() {
        let ledger = temp_path("incomplete-sidecars");
        fs::write(sidecar_path(&ledger, "team"), []).expect("create lone Team sidecar");
        let calls = Rc::new(Cell::new(0));
        let mut interaction = RecordingInteraction::approve();

        let result = ProductDriver::open_with_executor(
            &ledger,
            CountingEchoExecutor::new(calls),
            &mut interaction,
        );

        assert!(matches!(result, Err(ProductDriverError::IncompleteState)));
        assert!(!ledger.exists());
        assert!(!sidecar_path(&ledger, "tool").exists());
        fs::remove_file(sidecar_path(&ledger, "team")).expect("cleanup Team sidecar");
    }

    #[test]
    fn successful_effect_is_never_repeated_after_continuation_crash() {
        let ledger = temp_path("continuation-crash");
        let calls = Rc::new(Cell::new(0));
        let mut interaction = RecordingInteraction::approve();
        let mut driver = ProductDriver::open_with_executor(
            &ledger,
            CountingEchoExecutor::new(Rc::clone(&calls)),
            &mut interaction,
        )
        .expect("open Product driver");
        let mut provider = PanicAfterToolProvider;

        let crash = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = driver.execute(
                &ConfigLayers::default(),
                "crash after effect",
                &mut provider,
                &mut interaction,
            );
        }));
        assert!(crash.is_err());
        assert_eq!(calls.get(), 1);
        drop(driver);

        let mut interaction = RecordingInteraction::approve();
        let mut recovered = ProductDriver::open_with_executor(
            &ledger,
            CountingEchoExecutor::new(Rc::clone(&calls)),
            &mut interaction,
        )
        .expect("reopen Product driver");
        let mut provider = LocalEchoProvider::default();
        assert!(matches!(
            recovered.resume(&mut provider, &mut interaction),
            Err(ProductDriverError::Runtime(
                RuntimeError::ProviderToolResultUnavailable(_)
            ))
        ));
        assert_eq!(calls.get(), 1);
        assert!(matches!(
            recovered.snapshot().status,
            RecoveryStatus::Blocked { .. }
        ));
        drop(recovered);
        cleanup(&ledger);
    }

    #[test]
    fn ambiguous_effect_requires_reconciliation_and_is_never_repeated() {
        let ledger = temp_path("ambiguous-effect");
        let calls = Rc::new(Cell::new(0));
        let mut interaction = RecordingInteraction::approve();
        let mut driver = ProductDriver::open_with_executor(
            &ledger,
            AmbiguousEchoExecutor::new(Rc::clone(&calls)),
            &mut interaction,
        )
        .expect("open Product driver");
        let mut provider = LocalEchoProvider::default();

        assert!(matches!(
            driver.execute(
                &ConfigLayers::default(),
                "ambiguous effect",
                &mut provider,
                &mut interaction,
            ),
            Err(ProductDriverError::Runtime(
                RuntimeError::ToolReconciliationRequired(_)
            ))
        ));
        assert_eq!(calls.get(), 1);
        drop(driver);

        let mut interaction = RecordingInteraction::approve();
        let mut recovered = ProductDriver::open_with_executor(
            &ledger,
            AmbiguousEchoExecutor::new(Rc::clone(&calls)),
            &mut interaction,
        )
        .expect("reopen Product driver");
        let mut provider = LocalEchoProvider::default();
        assert!(matches!(
            recovered.resume(&mut provider, &mut interaction),
            Err(ProductDriverError::Runtime(
                RuntimeError::ToolReconciliationRequired(_)
            ))
        ));
        assert_eq!(calls.get(), 1);
        drop(recovered);
        cleanup(&ledger);
    }

    #[derive(Clone, Copy)]
    enum InteractionMode {
        Approve,
        Deny,
        FailApproval,
        FailTeam,
    }

    struct ApprovalView {
        tool: String,
        arguments: String,
    }

    struct RecordingInteraction {
        mode: InteractionMode,
        team_operations: usize,
        approvals: Vec<ApprovalView>,
    }

    impl RecordingInteraction {
        fn approve() -> Self {
            Self::new(InteractionMode::Approve)
        }

        fn deny() -> Self {
            Self::new(InteractionMode::Deny)
        }

        fn fail_approval() -> Self {
            Self::new(InteractionMode::FailApproval)
        }

        fn fail_team() -> Self {
            Self::new(InteractionMode::FailTeam)
        }

        fn new(mode: InteractionMode) -> Self {
            Self {
                mode,
                team_operations: 0,
                approvals: Vec::new(),
            }
        }
    }

    impl ProductInteraction for RecordingInteraction {
        fn present_team_operation(&mut self, _record: TeamOperationRecord) -> io::Result<()> {
            self.team_operations += 1;
            if matches!(self.mode, InteractionMode::FailTeam) {
                Err(io::Error::other("Team receipt interrupted"))
            } else {
                Ok(())
            }
        }

        fn decide_tool(
            &mut self,
            approval: &ProviderToolApproval,
        ) -> io::Result<ProductToolDecision> {
            self.approvals.push(ApprovalView {
                tool: approval.tool().to_owned(),
                arguments: approval.arguments().canonical_json().to_owned(),
            });
            match self.mode {
                InteractionMode::Approve => Ok(ProductToolDecision::Approve),
                InteractionMode::Deny => Ok(ProductToolDecision::Deny),
                InteractionMode::FailApproval => Err(io::Error::other("interaction interrupted")),
                InteractionMode::FailTeam => {
                    panic!("Team receipt failure cannot reach Tool approval")
                }
            }
        }
    }

    #[derive(Default)]
    struct LocalEchoProvider {
        runs: usize,
    }

    impl ProviderRuntime for LocalEchoProvider {
        fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            self.runs += 1;
            Ok(vec![
                ProviderEvent::FunctionCall(ProviderToolCall::new(
                    "call-product-echo-1",
                    "local.echo",
                    r#"{"message":"approved message"}"#,
                )?),
                ProviderEvent::Completed(UsageRecord::default()),
            ])
        }

        fn continue_after_tool(
            &mut self,
            _request: &ProviderRequest,
            output: &ProviderToolOutput,
        ) -> Result<Vec<ProviderEvent>, ProviderError> {
            assert_eq!(output.call_id(), "call-product-echo-1");
            assert_eq!(output.output(), "approved message");
            Ok(vec![
                ProviderEvent::TextDelta("Echoed: approved message".into()),
                ProviderEvent::Completed(UsageRecord::default()),
            ])
        }
    }

    struct PanicAfterToolProvider;

    impl ProviderRuntime for PanicAfterToolProvider {
        fn run(&mut self, _request: &ProviderRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![
                ProviderEvent::FunctionCall(ProviderToolCall::new(
                    "call-product-echo-1",
                    "local.echo",
                    r#"{"message":"approved message"}"#,
                )?),
                ProviderEvent::Completed(UsageRecord::default()),
            ])
        }

        fn continue_after_tool(
            &mut self,
            _request: &ProviderRequest,
            _output: &ProviderToolOutput,
        ) -> Result<Vec<ProviderEvent>, ProviderError> {
            panic!("injected crash after durable Tool success")
        }
    }

    struct CountingEchoExecutor {
        calls: Rc<Cell<usize>>,
    }

    impl CountingEchoExecutor {
        fn new(calls: Rc<Cell<usize>>) -> Self {
            Self { calls }
        }
    }

    impl ToolEffectExecutor for CountingEchoExecutor {
        fn execute(&mut self, call: &AuthorizedToolCall<'_>) -> ToolExecution {
            self.calls.set(self.calls.get() + 1);
            assert_eq!(call.tool(), "local.echo");
            assert_eq!(
                call.arguments().canonical_json(),
                r#"{"message":"approved message"}"#
            );
            ToolExecution::Succeeded {
                output: b"approved message".to_vec(),
            }
        }
    }

    struct AmbiguousEchoExecutor {
        calls: Rc<Cell<usize>>,
    }

    impl AmbiguousEchoExecutor {
        fn new(calls: Rc<Cell<usize>>) -> Self {
            Self { calls }
        }
    }

    impl ToolEffectExecutor for AmbiguousEchoExecutor {
        fn execute(&mut self, _call: &AuthorizedToolCall<'_>) -> ToolExecution {
            self.calls.set(self.calls.get() + 1);
            ToolExecution::Ambiguous {
                reason: "injected ambiguous effect".into(),
            }
        }
    }

    fn temp_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "greentyper-product-driver-{name}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn cleanup(runtime: &PathBuf) {
        fs::remove_file(runtime).expect("cleanup Runtime Ledger");
        fs::remove_file(sidecar_path(runtime, "team")).expect("cleanup Team Ledger");
        fs::remove_file(sidecar_path(runtime, "tool")).expect("cleanup Tool Ledger");
    }
}
