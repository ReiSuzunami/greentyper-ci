//! Terminal-neutral Config editor sessions over schema-owned drafts.

use std::error::Error;
use std::fmt;

use serde::Serialize;

use super::{
    CommandQueryError, CommandTarget, ConfigChange, ConfigCommit, ConfigDraft, ConfigFieldContents,
    ConfigFieldView, ConfigObjectRef, ConfigRevision, ConfigRuntime, ConfigRuntimeError,
    ConfigScope, match_command_paths,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigEditorView {
    pub base_revision: ConfigRevision,
    pub field: ConfigFieldView,
    pub changes: Vec<ConfigChange>,
}

pub struct ConfigEditorSession {
    draft: ConfigDraft,
    field_path: String,
    credential_binding: bool,
}

impl fmt::Debug for ConfigEditorSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigEditorSession")
            .field("scope", &self.draft.scope())
            .field("base_revision", &self.draft.base_revision())
            .field("field_path", &self.field_path)
            .field("credential_binding", &self.credential_binding)
            .finish_non_exhaustive()
    }
}

impl ConfigEditorSession {
    pub fn open_from_query(
        runtime: &ConfigRuntime,
        scope: ConfigScope,
        query: &str,
        selected: usize,
        object: Option<&ConfigObjectRef>,
    ) -> Result<Self, ConfigEditorError> {
        let matches = match_command_paths(query)?;
        let matched = matches
            .get(selected)
            .ok_or(ConfigEditorError::NoCommandMatch)?;
        let CommandTarget::ConfigEditor { path_pattern, .. } = matched.path().target() else {
            return Err(ConfigEditorError::CommandTargetNotEditor);
        };
        let field = if path_pattern.contains("<id>") {
            let object = object.ok_or(ConfigEditorError::ConfigObjectRequired)?;
            runtime
                .object_fields(scope, object.kind(), object.id())?
                .into_iter()
                .find(|field| field.path_pattern == path_pattern)
                .ok_or(ConfigEditorError::ConfigObjectMismatch)?
        } else {
            runtime.inspect_field(scope, path_pattern)?
        };
        let credential_binding = matches!(
            field.contents,
            ConfigFieldContents::CredentialBinding { .. }
        );
        Ok(Self {
            draft: runtime.begin_draft(scope)?,
            field_path: field.path,
            credential_binding,
        })
    }

    pub fn stage_raw(&mut self, raw: &str) -> Result<(), ConfigEditorError> {
        self.require_value_editor()?;
        self.draft.set_raw(&self.field_path, raw)?;
        Ok(())
    }

    pub fn reset(&mut self) -> Result<(), ConfigEditorError> {
        self.require_value_editor()?;
        self.draft.reset(&self.field_path)?;
        Ok(())
    }

    pub fn preview(
        &self,
        runtime: &mut ConfigRuntime,
    ) -> Result<ConfigEditorView, ConfigEditorError> {
        let preview = runtime.commit(self.draft.clone(), true)?;
        Ok(ConfigEditorView {
            base_revision: self.draft.base_revision(),
            field: runtime.inspect_draft_field(&self.draft, &self.field_path)?,
            changes: preview.changes,
        })
    }

    pub fn commit(self, runtime: &mut ConfigRuntime) -> Result<ConfigCommit, ConfigEditorError> {
        self.require_value_editor()?;
        runtime.commit(self.draft, false).map_err(Into::into)
    }

    fn require_value_editor(&self) -> Result<(), ConfigEditorError> {
        if self.credential_binding {
            Err(ConfigEditorError::CredentialOperationRequired)
        } else {
            Ok(())
        }
    }
}

#[derive(Debug)]
pub enum ConfigEditorError {
    Command(CommandQueryError),
    Config(ConfigRuntimeError),
    NoCommandMatch,
    CommandTargetNotEditor,
    ConfigObjectRequired,
    ConfigObjectMismatch,
    CredentialOperationRequired,
}

impl fmt::Display for ConfigEditorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Command(source) => write!(formatter, "{source}"),
            Self::Config(source) => write!(formatter, "{source}"),
            Self::NoCommandMatch => formatter.write_str("command query has no matching action"),
            Self::CommandTargetNotEditor => {
                formatter.write_str("selected command does not open a Config editor")
            }
            Self::ConfigObjectRequired => {
                formatter.write_str("Config editor requires an object selection")
            }
            Self::ConfigObjectMismatch => {
                formatter.write_str("selected Config object does not own this field")
            }
            Self::CredentialOperationRequired => {
                formatter.write_str("credential binding requires a secure credential operation")
            }
        }
    }
}

impl Error for ConfigEditorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Command(source) => Some(source),
            Self::Config(source) => Some(source),
            Self::NoCommandMatch
            | Self::CommandTargetNotEditor
            | Self::ConfigObjectRequired
            | Self::ConfigObjectMismatch
            | Self::CredentialOperationRequired => None,
        }
    }
}

impl From<CommandQueryError> for ConfigEditorError {
    fn from(source: CommandQueryError) -> Self {
        Self::Command(source)
    }
}

impl From<ConfigRuntimeError> for ConfigEditorError {
    fn from(source: ConfigRuntimeError) -> Self {
        Self::Config(source)
    }
}
