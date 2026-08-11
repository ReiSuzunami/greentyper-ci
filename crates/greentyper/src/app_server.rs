use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, Write};

use greentyper_core::config::{
    CONFIG_FILE_SCHEMA_VERSION, ConfigCommit, ConfigDraft, ConfigErrorCategory, ConfigRuntime,
    ConfigRuntimeError, ConfigScope, ConfigValue, MAX_CONFIG_STRING_BYTES, config_schema,
};
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_DRAFTS: usize = 64;

pub(crate) fn run_stdio(
    mut input: impl BufRead,
    mut output: impl Write,
    config: ConfigRuntime,
) -> Result<(), AppServerError> {
    let mut server = AppServer::new(config);
    loop {
        let response = match read_request_line(&mut input)? {
            RequestLine::End => return Ok(()),
            RequestLine::TooLong => error_response(
                None,
                "request_too_large",
                "request exceeds the maximum frame size",
                None,
            ),
            RequestLine::Value(line) => server.handle(&line),
        };
        serde_json::to_writer(&mut output, &response)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}

struct AppServer {
    config: ConfigRuntime,
    drafts: BTreeMap<u64, ConfigDraft>,
    next_draft_id: u64,
}

impl AppServer {
    fn new(config: ConfigRuntime) -> Self {
        Self {
            config,
            drafts: BTreeMap::new(),
            next_draft_id: 1,
        }
    }

    fn handle(&mut self, line: &[u8]) -> Value {
        let request = match serde_json::from_slice::<Request>(line) {
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
                let Some(next_draft_id) = self.next_draft_id.checked_add(1) else {
                    return error_response(
                        Some(request.id),
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
                    request.id,
                    json!({
                        "draft_id": draft_id,
                        "scope": scope,
                        "base_revision": base_revision,
                    }),
                )
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: u64,
    operation: String,
    #[serde(default = "empty_params")]
    params: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

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

fn empty_params() -> Value {
    json!({})
}

fn parse_params<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, ()> {
    serde_json::from_value(params).map_err(|_| ())
}

fn valid_config_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= MAX_CONFIG_STRING_BYTES && !path.chars().any(char::is_control)
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
