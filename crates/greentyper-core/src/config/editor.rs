//! Terminal-neutral Config editor sessions over schema-owned drafts.

use std::error::Error;
use std::fmt;

use serde::Serialize;

use crate::provider::ProviderProfileSnapshot;

use super::{
    CommandQueryError, CommandTarget, ConfigChange, ConfigCommit, ConfigDraft, ConfigFieldContents,
    ConfigFieldInteraction, ConfigFieldView, ConfigObjectKind, ConfigObjectRef, ConfigRevision,
    ConfigRuntime, ConfigRuntimeError, ConfigScope, match_command_paths,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConfigEditorView {
    pub base_revision: ConfigRevision,
    pub field: ConfigFieldView,
    pub changes: Vec<ConfigChange>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigEditorOperation {
    Edit,
    Create,
    Delete,
}

pub struct ConfigEditorSession {
    draft: ConfigDraft,
    field_path: String,
    credential_binding: bool,
    operation: ConfigEditorOperation,
    object: Option<ConfigObjectRef>,
}

impl fmt::Debug for ConfigEditorSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigEditorSession")
            .field("scope", &self.draft.scope())
            .field("base_revision", &self.draft.base_revision())
            .field("field_path", &self.field_path)
            .field("credential_binding", &self.credential_binding)
            .field("operation", &self.operation)
            .field("object", &self.object)
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
            operation: ConfigEditorOperation::Edit,
            object: object.cloned(),
        })
    }

    pub fn create_object(
        runtime: &ConfigRuntime,
        scope: ConfigScope,
        object: ConfigObjectRef,
    ) -> Result<Self, ConfigEditorError> {
        let draft = runtime.begin_draft(scope)?;
        if draft.contains_object(&object)? {
            return Err(ConfigEditorError::ConfigObjectAlreadyExists);
        }
        Self::create_with_draft(runtime, draft, object)
    }

    pub fn create_model_starter(
        runtime: &ConfigRuntime,
        scope: ConfigScope,
        object: ConfigObjectRef,
        provider: &str,
        catalog_key: &str,
    ) -> Result<Self, ConfigEditorError> {
        if object.kind() != ConfigObjectKind::ModelPreset {
            return Err(ConfigEditorError::ConfigObjectMismatch);
        }
        let draft = runtime.begin_model_starter(scope, object.id(), provider, catalog_key)?;
        Self::create_with_draft(runtime, draft, object)
    }

    pub fn create_model_preset(
        runtime: &ConfigRuntime,
        scope: ConfigScope,
        object: ConfigObjectRef,
        provider: &str,
        model: &str,
        dialect: crate::provider::ProviderDialect,
    ) -> Result<Self, ConfigEditorError> {
        if object.kind() != ConfigObjectKind::ModelPreset {
            return Err(ConfigEditorError::ConfigObjectMismatch);
        }
        let draft = runtime.begin_model_preset(scope, object.id(), provider, model, dialect)?;
        Self::create_with_draft(runtime, draft, object)
    }

    fn create_with_draft(
        runtime: &ConfigRuntime,
        draft: ConfigDraft,
        object: ConfigObjectRef,
    ) -> Result<Self, ConfigEditorError> {
        let fields = runtime.draft_object_fields(&draft, object.kind(), object.id())?;
        let field = fields
            .iter()
            .find(|field| field.interaction != ConfigFieldInteraction::ReadOnly)
            .or_else(|| fields.first())
            .cloned()
            .ok_or(ConfigEditorError::ConfigObjectMismatch)?;
        let credential_binding = matches!(
            field.contents,
            ConfigFieldContents::CredentialBinding { .. }
        );
        Ok(Self {
            draft,
            field_path: field.path,
            credential_binding,
            operation: ConfigEditorOperation::Create,
            object: Some(object),
        })
    }

    pub fn delete_object(
        runtime: &ConfigRuntime,
        scope: ConfigScope,
        object: ConfigObjectRef,
    ) -> Result<Self, ConfigEditorError> {
        let mut draft = runtime.begin_draft(scope)?;
        if !draft.contains_object(&object)? {
            return Err(ConfigEditorError::ConfigObjectNotInTargetScope);
        }
        let field = runtime
            .draft_object_fields(&draft, object.kind(), object.id())?
            .into_iter()
            .next()
            .ok_or(ConfigEditorError::ConfigObjectMismatch)?;
        draft.delete_object(&object)?;
        Ok(Self {
            draft,
            field_path: field.path,
            credential_binding: false,
            operation: ConfigEditorOperation::Delete,
            object: Some(object),
        })
    }

    #[must_use]
    pub const fn operation(&self) -> ConfigEditorOperation {
        self.operation
    }

    #[must_use]
    pub fn object(&self) -> Option<&ConfigObjectRef> {
        self.object.as_ref()
    }

    pub fn stage_raw(&mut self, raw: &str) -> Result<(), ConfigEditorError> {
        self.require_value_editor()?;
        self.draft.set_raw(&self.field_path, raw)?;
        Ok(())
    }

    pub fn stage_credential_reference(&mut self, reference: &str) -> Result<(), ConfigEditorError> {
        if self.operation == ConfigEditorOperation::Delete {
            return Err(ConfigEditorError::ObjectDeletionStaged);
        }
        if !self.credential_binding {
            return Err(ConfigEditorError::CredentialBindingRequired);
        }
        self.draft.set_raw(&self.field_path, reference)?;
        Ok(())
    }

    pub fn reset_credential_reference(&mut self) -> Result<(), ConfigEditorError> {
        if self.operation == ConfigEditorOperation::Delete {
            return Err(ConfigEditorError::ObjectDeletionStaged);
        }
        if !self.credential_binding {
            return Err(ConfigEditorError::CredentialBindingRequired);
        }
        self.draft.reset(&self.field_path)?;
        Ok(())
    }

    pub fn focus_from_query(
        &mut self,
        runtime: &ConfigRuntime,
        query: &str,
        selected: usize,
    ) -> Result<(), ConfigEditorError> {
        if self.operation == ConfigEditorOperation::Delete {
            return Err(ConfigEditorError::ObjectDeletionStaged);
        }
        let matches = match_command_paths(query)?;
        let matched = matches
            .get(selected)
            .ok_or(ConfigEditorError::NoCommandMatch)?;
        let CommandTarget::ConfigEditor { path_pattern, .. } = matched.path().target() else {
            return Err(ConfigEditorError::CommandTargetNotEditor);
        };
        let field = match &self.object {
            Some(object) => runtime
                .draft_object_fields(&self.draft, object.kind(), object.id())?
                .into_iter()
                .find(|field| field.path_pattern == path_pattern)
                .ok_or(ConfigEditorError::ConfigObjectMismatch)?,
            None if !path_pattern.contains("<id>") => {
                runtime.inspect_draft_field(&self.draft, path_pattern)?
            }
            None => return Err(ConfigEditorError::ConfigObjectRequired),
        };
        self.credential_binding = matches!(
            field.contents,
            ConfigFieldContents::CredentialBinding { .. }
        );
        self.field_path = field.path;
        Ok(())
    }

    pub fn move_field(
        &mut self,
        runtime: &ConfigRuntime,
        offset: isize,
    ) -> Result<(), ConfigEditorError> {
        if self.operation == ConfigEditorOperation::Delete {
            return Err(ConfigEditorError::ObjectDeletionStaged);
        }
        let Some(object) = self.object.as_ref() else {
            return Ok(());
        };
        let fields = runtime
            .draft_object_fields(&self.draft, object.kind(), object.id())?
            .into_iter()
            .filter(|field| field.interaction != ConfigFieldInteraction::ReadOnly)
            .collect::<Vec<_>>();
        let Some(index) = fields
            .iter()
            .position(|field| field.path == self.field_path)
        else {
            return Ok(());
        };
        let next = index
            .saturating_add_signed(offset)
            .min(fields.len().saturating_sub(1));
        let Some(field) = fields.get(next) else {
            return Ok(());
        };
        self.credential_binding = matches!(
            field.contents,
            ConfigFieldContents::CredentialBinding { .. }
        );
        self.field_path.clone_from(&field.path);
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

    pub fn current_view(
        &self,
        runtime: &ConfigRuntime,
    ) -> Result<ConfigEditorView, ConfigEditorError> {
        Ok(ConfigEditorView {
            base_revision: self.draft.base_revision(),
            field: runtime.inspect_draft_field(&self.draft, &self.field_path)?,
            changes: Vec::new(),
        })
    }

    pub fn field(&self, runtime: &ConfigRuntime) -> Result<ConfigFieldView, ConfigEditorError> {
        runtime
            .inspect_draft_field(&self.draft, &self.field_path)
            .map_err(Into::into)
    }

    pub fn provider_profile(
        &self,
        runtime: &ConfigRuntime,
    ) -> Result<ProviderProfileSnapshot, ConfigEditorError> {
        if self.operation == ConfigEditorOperation::Delete {
            return Err(ConfigEditorError::ObjectDeletionStaged);
        }
        let object = self
            .object
            .as_ref()
            .filter(|object| object.kind() == ConfigObjectKind::ProviderProfile)
            .ok_or(ConfigEditorError::ProviderProfileRequired)?;
        runtime
            .provider_profile_for_draft(&self.draft, object.id())
            .map_err(Into::into)
    }

    pub fn commit(self, runtime: &mut ConfigRuntime) -> Result<ConfigCommit, ConfigEditorError> {
        self.require_commit_allowed()?;
        runtime.commit(self.draft, false).map_err(Into::into)
    }

    /// Attempts the commit without consuming this session when validation or CAS fails.
    pub fn try_commit(
        &self,
        runtime: &mut ConfigRuntime,
    ) -> Result<ConfigCommit, ConfigEditorError> {
        self.require_commit_allowed()?;
        runtime
            .commit(self.draft.clone(), false)
            .map_err(Into::into)
    }

    pub fn try_commit_provider_profile(
        &self,
        runtime: &mut ConfigRuntime,
    ) -> Result<ConfigCommit, ConfigEditorError> {
        if self.operation == ConfigEditorOperation::Delete {
            return Err(ConfigEditorError::ObjectDeletionStaged);
        }
        match self.require_commit_allowed() {
            Ok(()) => {}
            Err(ConfigEditorError::CredentialOperationRequired) if self.credential_binding => {}
            Err(source) => return Err(source),
        }
        self.object
            .as_ref()
            .filter(|object| object.kind() == ConfigObjectKind::ProviderProfile)
            .ok_or(ConfigEditorError::ProviderProfileRequired)?;
        runtime
            .commit(self.draft.clone(), false)
            .map_err(Into::into)
    }

    fn require_value_editor(&self) -> Result<(), ConfigEditorError> {
        if self.operation == ConfigEditorOperation::Delete {
            Err(ConfigEditorError::ObjectDeletionStaged)
        } else if self.credential_binding {
            Err(ConfigEditorError::CredentialOperationRequired)
        } else {
            Ok(())
        }
    }

    fn require_commit_allowed(&self) -> Result<(), ConfigEditorError> {
        if self.operation == ConfigEditorOperation::Delete {
            Ok(())
        } else {
            self.require_value_editor()
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
    ConfigObjectAlreadyExists,
    ConfigObjectNotInTargetScope,
    ObjectDeletionStaged,
    CredentialOperationRequired,
    CredentialBindingRequired,
    ProviderProfileRequired,
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
            Self::ConfigObjectAlreadyExists => formatter.write_str("Config object already exists"),
            Self::ConfigObjectNotInTargetScope => {
                formatter.write_str("Config object does not exist in the target layer")
            }
            Self::ObjectDeletionStaged => {
                formatter.write_str("Config object deletion does not accept field mutations")
            }
            Self::CredentialOperationRequired => {
                formatter.write_str("credential binding requires a secure credential operation")
            }
            Self::CredentialBindingRequired => {
                formatter.write_str("secure credential reference requires a credential field")
            }
            Self::ProviderProfileRequired => {
                formatter.write_str("connection test requires a Provider Profile editor")
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
            | Self::ConfigObjectAlreadyExists
            | Self::ConfigObjectNotInTargetScope
            | Self::ObjectDeletionStaged
            | Self::CredentialOperationRequired
            | Self::CredentialBindingRequired
            | Self::ProviderProfileRequired => None,
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
