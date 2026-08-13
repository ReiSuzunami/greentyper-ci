//! Product-owned, bounded project Skill discovery and execution.
//!
//! Skills describe a workflow; they never grant capabilities.  This first
//! slice deliberately maps one Skill to the existing `local.echo` Tool only.

use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use greentyper_core::agent_team::{
    Capability, CapabilitySnapshot, CommandOutcome, ResourceBudget, TaskScope, TaskSpec,
    TeamCommand,
};
use greentyper_core::runtime::RuntimeKernel;
use greentyper_core::tool_runtime::{
    ApprovalDecision, ToolArguments, ToolCallOutcome, ToolEffectExecutor, ToolIntent,
    ToolRequestOutcome, ToolResources,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::local_process::{LOCAL_ECHO_PROCESS, LOCAL_ECHO_TOOL, LocalProcessExecutor};

const MAX_SKILLS: usize = 64;
const MAX_SKILL_ID_BYTES: usize = 64;
const MAX_SKILL_NAME_BYTES: usize = 256;
const MAX_SKILL_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_SKILL_FILE_BYTES: usize = 256 * 1024;
const SKILL_FILE: &str = "skill.toml";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct SkillSummary {
    pub id: String,
    pub name: String,
    pub description: String,
    pub source: String,
    pub content_sha256: String,
    pub tool: &'static str,
}

#[derive(Debug)]
pub(crate) enum SkillError {
    Io(io::Error),
    Invalid(String),
    Unknown(String),
    Runtime(greentyper_core::runtime::RuntimeError),
    Tool(greentyper_core::tool_runtime::ToolRuntimeError),
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => write!(f, "Skill I/O failed"),
            Self::Invalid(reason) => write!(f, "invalid Skill: {reason}"),
            Self::Unknown(id) => write!(f, "unknown Skill {id}"),
            Self::Runtime(source) => write!(f, "{source}"),
            Self::Tool(source) => write!(f, "{source}"),
        }
    }
}

impl Error for SkillError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(source) => Some(source),
            Self::Runtime(source) => Some(source),
            Self::Tool(source) => Some(source),
            Self::Invalid(_) | Self::Unknown(_) => None,
        }
    }
}

impl From<io::Error> for SkillError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

impl From<greentyper_core::runtime::RuntimeError> for SkillError {
    fn from(source: greentyper_core::runtime::RuntimeError) -> Self {
        Self::Runtime(source)
    }
}

impl From<greentyper_core::tool_runtime::ToolRuntimeError> for SkillError {
    fn from(source: greentyper_core::tool_runtime::ToolRuntimeError) -> Self {
        Self::Tool(source)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillManifest {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    tool: String,
    message: String,
}

#[derive(Clone, Debug)]
struct LoadedSkill {
    summary: SkillSummary,
    message: String,
}

pub(crate) fn list_skills(project_root: &Path) -> Result<Vec<SkillSummary>, SkillError> {
    let skills_root = project_root.join(".greentyper").join("skills");
    if !skills_root.exists() {
        return Ok(Vec::new());
    }
    let mut entries = fs::read_dir(&skills_root)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    if entries.len() > MAX_SKILLS {
        return Err(SkillError::Invalid("too many Skills".into()));
    }
    entries
        .into_iter()
        .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
        .map(|entry| load_skill(&entry.path()).map(|skill| skill.summary))
        .collect()
}

fn load_named_skill(project_root: &Path, id: &str) -> Result<LoadedSkill, SkillError> {
    validate_id(id)?;
    let path = project_root.join(".greentyper").join("skills").join(id);
    if !path.is_dir() {
        return Err(SkillError::Unknown(id.to_owned()));
    }
    let skill = load_skill(&path)?;
    if skill.summary.id != id {
        return Err(SkillError::Invalid(
            "manifest id does not match its directory".into(),
        ));
    }
    Ok(skill)
}

pub(crate) fn run_skill(
    project_root: &Path,
    runtime_path: &Path,
    id: &str,
    message_override: Option<&str>,
    approve: bool,
) -> Result<serde_json::Value, SkillError> {
    let mut executor =
        LocalProcessExecutor::current().map_err(|error| SkillError::Invalid(error.to_string()))?;
    run_skill_with_executor(
        project_root,
        runtime_path,
        id,
        message_override,
        approve,
        &mut executor,
    )
}

pub(crate) fn run_skill_with_executor<E: ToolEffectExecutor>(
    project_root: &Path,
    runtime_path: &Path,
    id: &str,
    message_override: Option<&str>,
    approve: bool,
    executor: &mut E,
) -> Result<serde_json::Value, SkillError> {
    let skill = load_named_skill(project_root, id)?;
    if skill.summary.tool != LOCAL_ECHO_TOOL {
        return Err(SkillError::Invalid(
            "only local.echo Skill execution is supported".into(),
        ));
    }
    if !approve {
        return Err(SkillError::Invalid(
            "Skill execution requires explicit approval".into(),
        ));
    }
    let message = message_override.unwrap_or(&skill.message);
    validate_message(message)?;
    if let Some(parent) = runtime_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let (mut kernel, recovery) = RuntimeKernel::open_with_team_and_tools(
        runtime_path,
        sidecar_path(runtime_path, "team"),
        sidecar_path(runtime_path, "tool"),
        1,
    )?;
    let mut sessions = recovery.into_sessions();
    let session = if sessions.len() > 1 {
        return Err(SkillError::Invalid(
            "multiple active Skill Agents are unsupported".into(),
        ));
    } else if let Some(session) = sessions.pop() {
        session
    } else {
        let operation = kernel.dispatch_team(TeamCommand::AdmitRoot {
            task: TaskSpec::new(
                "run one approved project Skill",
                TaskScope::from_labels(["skill"]),
            ),
            budget: ResourceBudget::new(1_000, 1),
            capabilities: CapabilitySnapshot::from_capabilities([
                Capability::Tool(LOCAL_ECHO_TOOL.into()),
                Capability::Process,
            ]),
        })?;
        kernel.acknowledge_team_operation(operation.operation)?;
        match operation.commit.outcome {
            CommandOutcome::RootAdmitted { session, .. } => session,
            _ => return Err(SkillError::Invalid("Skill root admission failed".into())),
        }
    };
    let arguments = ToolArguments::parse(&serde_json::json!({"message": message}).to_string())?;
    let intent = ToolIntent::new(
        format!(
            "skill:{}:{}",
            skill.summary.id, skill.summary.content_sha256
        ),
        LOCAL_ECHO_TOOL,
        arguments,
        ToolResources::default().with_process(LOCAL_ECHO_PROCESS),
    )?;
    let request = match kernel.request_tool_call(session, intent)? {
        ToolRequestOutcome::ApprovalRequired(request) => request,
        ToolRequestOutcome::Existing(record) => {
            return Ok(serde_json::json!({
                "skill": skill.summary,
                "status": format_tool_status(record.status),
                "call": record.call.get(),
                "reused": true,
            }));
        }
    };
    let outcome = kernel.resolve_tool_call(
        request,
        ApprovalDecision::Grant {
            expires_at_unix_ms: u64::MAX,
        },
        executor,
    )?;
    match outcome {
        ToolCallOutcome::Succeeded { record, output } => Ok(serde_json::json!({
            "skill": skill.summary,
            "status": "succeeded",
            "call": record.call.get(),
            "output": String::from_utf8_lossy(&output),
            "result_sha256": record.result_digest.map(encode_hash),
            "reused": false,
        })),
        ToolCallOutcome::Failed(record) => Ok(serde_json::json!({
            "skill": skill.summary, "status": "failed", "call": record.call.get(),
        })),
        ToolCallOutcome::Denied(record) => Ok(serde_json::json!({
            "skill": skill.summary, "status": "denied", "call": record.call.get(),
        })),
        ToolCallOutcome::ReconciliationRequired(record) => Ok(serde_json::json!({
            "skill": skill.summary, "status": "reconciliation_required", "call": record.call.get(),
        })),
    }
}

fn load_skill(path: &Path) -> Result<LoadedSkill, SkillError> {
    let manifest_path = path.join(SKILL_FILE);
    let bytes = fs::read(&manifest_path)?;
    if bytes.len() > MAX_SKILL_FILE_BYTES {
        return Err(SkillError::Invalid(
            "manifest exceeds the byte limit".into(),
        ));
    }
    let digest = Sha256::digest(&bytes);
    let manifest: SkillManifest = toml::from_slice(&bytes)
        .map_err(|_| SkillError::Invalid("manifest is not valid TOML".into()))?;
    validate_id(&manifest.id)?;
    validate_text(&manifest.name, MAX_SKILL_NAME_BYTES, "name")?;
    validate_text(&manifest.description, MAX_SKILL_NAME_BYTES, "description")?;
    validate_message(&manifest.message)?;
    if manifest.tool != LOCAL_ECHO_TOOL {
        return Err(SkillError::Invalid(
            "manifest tool must be local.echo".into(),
        ));
    }
    Ok(LoadedSkill {
        summary: SkillSummary {
            id: manifest.id,
            name: manifest.name,
            description: manifest.description,
            source: "project".to_owned(),
            content_sha256: encode_hash(digest.into()),
            tool: LOCAL_ECHO_TOOL,
        },
        message: manifest.message,
    })
}

fn validate_id(id: &str) -> Result<(), SkillError> {
    validate_text(id, MAX_SKILL_ID_BYTES, "id")?;
    if !id
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(SkillError::Invalid(
            "id must be lowercase kebab-case".into(),
        ));
    }
    Ok(())
}

fn validate_message(message: &str) -> Result<(), SkillError> {
    validate_text(message, MAX_SKILL_MESSAGE_BYTES, "message")
}

fn validate_text(value: &str, max: usize, field: &'static str) -> Result<(), SkillError> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        return Err(SkillError::Invalid(format!("{field} is invalid")));
    }
    Ok(())
}

fn sidecar_path(runtime: &Path, kind: &str) -> PathBuf {
    let mut path = runtime.as_os_str().to_owned();
    path.push(".");
    path.push(kind);
    PathBuf::from(path)
}

fn format_tool_status(status: greentyper_core::tool_runtime::ToolCallStatus) -> &'static str {
    match status {
        greentyper_core::tool_runtime::ToolCallStatus::AwaitingApproval => "awaiting_approval",
        greentyper_core::tool_runtime::ToolCallStatus::Denied => "denied",
        greentyper_core::tool_runtime::ToolCallStatus::ReconciliationRequired => {
            "reconciliation_required"
        }
        greentyper_core::tool_runtime::ToolCallStatus::Succeeded => "succeeded",
        greentyper_core::tool_runtime::ToolCallStatus::Failed => "failed",
    }
}

fn encode_hash(digest: [u8; 32]) -> String {
    digest
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
