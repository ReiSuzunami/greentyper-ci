use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::io::{self, BufRead, Write};

use greentyper_core::config::{
    CONFIG_FILE_SCHEMA_VERSION, ConfigCommit, ConfigDraft, ConfigErrorCategory, ConfigRuntime,
    ConfigRuntimeError, ConfigScope, ConfigValue, MAX_CONFIG_STRING_BYTES, config_schema,
};
use serde::{Deserialize, Deserializer};
use serde_json::value::RawValue;
use serde_json::{Value, json};

use crate::credential_vault::{
    CredentialVault, CredentialVaultError, PlatformCredentialVault, ProviderCredentialScope,
    SecretValue,
};

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_DRAFTS: usize = 64;

pub(crate) fn run_stdio(
    input: impl BufRead,
    output: impl Write,
    config: ConfigRuntime,
) -> Result<(), AppServerError> {
    let mut vault = PlatformCredentialVault;
    run_stdio_with_vault(input, output, config, &mut vault)
}

fn run_stdio_with_vault(
    mut input: impl BufRead,
    mut output: impl Write,
    config: ConfigRuntime,
    vault: &mut impl CredentialVault,
) -> Result<(), AppServerError> {
    let mut server = AppServer::new(config, vault);
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

struct AppServer<'vault, V> {
    config: ConfigRuntime,
    drafts: BTreeMap<u64, ConfigDraft>,
    next_draft_id: u64,
    vault: &'vault mut V,
}

impl<'vault, V: CredentialVault> AppServer<'vault, V> {
    fn new(config: ConfigRuntime, vault: &'vault mut V) -> Self {
        Self {
            config,
            drafts: BTreeMap::new(),
            next_draft_id: 1,
            vault,
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
    use std::io::Cursor;
    use std::time::{SystemTime, UNIX_EPOCH};

    use greentyper_core::config::{ConfigDocument, ConfigPaths, ConfigRuntime};
    use serde_json::{Value, json};

    use super::run_stdio_with_vault;
    use crate::credential_vault::{
        CredentialVault, InMemoryCredentialVault, ProviderCredentialScope,
    };

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
}
