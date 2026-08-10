//! Provider-neutral connection-test status with the current bounded HTTP models probe.

use std::collections::BTreeSet;
use std::io::Read;
use std::time::Duration;

use greentyper_core::provider::ProviderProfileSnapshot;
use greentyper_core::provider_catalog::ProviderCatalog;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

use crate::credential_vault::{CredentialVault, CredentialVaultError, ProviderCredentialScope};
use crate::provider_http_policy::{bearer_header, validate_provider_endpoint};

pub(crate) const MAX_MODELS_RESPONSE_BYTES: usize = 256 * 1024;
pub(crate) const MAX_OBSERVED_MODELS: usize = 1024;
pub(crate) const MAX_MODEL_ID_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct ObservedProviderModel {
    pub(crate) id: String,
    pub(crate) release_catalog_key: Option<String>,
}

#[derive(Deserialize)]
struct ModelsResponse {
    data: Vec<ModelsResponseEntry>,
}

#[derive(Deserialize)]
struct ModelsResponseEntry {
    id: String,
}

#[derive(Clone, Copy)]
enum ModelsResponseError {
    Unavailable,
    InvalidResponse,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProviderConnectionFailureCategory {
    MissingModelsRoute,
    InvalidConfiguration,
    CredentialMissing,
    CredentialUnavailable,
    CredentialRejected,
    RequestRejected,
    Unavailable,
    InvalidResponse,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum ProviderConnectionTestStatus {
    Untested,
    Succeeded {
        profile: String,
        fingerprint: u64,
        models: Vec<ObservedProviderModel>,
    },
    Failed {
        category: ProviderConnectionFailureCategory,
        retryable: bool,
    },
}

pub(crate) trait ProviderConnectionTester {
    fn test(&mut self, profile: &ProviderProfileSnapshot) -> ProviderConnectionTestStatus;
}

pub(crate) struct ModelsHttpConnectionTester<'a, V> {
    vault: &'a V,
    timeout: Duration,
}

impl<'a, V: CredentialVault> ModelsHttpConnectionTester<'a, V> {
    pub(crate) const fn new(vault: &'a V) -> Self {
        Self {
            vault,
            timeout: Duration::from_secs(10),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeout(vault: &'a V, timeout: Duration) -> Self {
        Self { vault, timeout }
    }

    fn failed(
        category: ProviderConnectionFailureCategory,
        retryable: bool,
    ) -> ProviderConnectionTestStatus {
        ProviderConnectionTestStatus::Failed {
            category,
            retryable,
        }
    }

    fn parse_models(
        profile: &ProviderProfileSnapshot,
        response: reqwest::blocking::Response,
    ) -> Result<Vec<ObservedProviderModel>, ModelsResponseError> {
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json")) {
            return Err(ModelsResponseError::InvalidResponse);
        }
        let max_response_bytes = u64::try_from(MAX_MODELS_RESPONSE_BYTES)
            .map_err(|_| ModelsResponseError::InvalidResponse)?;
        if response
            .content_length()
            .is_some_and(|length| length > max_response_bytes)
        {
            return Err(ModelsResponseError::InvalidResponse);
        }
        let read_limit = max_response_bytes
            .checked_add(1)
            .ok_or(ModelsResponseError::InvalidResponse)?;
        let mut body = Vec::new();
        response
            .take(read_limit)
            .read_to_end(&mut body)
            .map_err(|_| ModelsResponseError::Unavailable)?;
        if body.len() > MAX_MODELS_RESPONSE_BYTES {
            return Err(ModelsResponseError::InvalidResponse);
        }
        let decoded: ModelsResponse =
            serde_json::from_slice(&body).map_err(|_| ModelsResponseError::InvalidResponse)?;
        if decoded.data.len() > MAX_OBSERVED_MODELS {
            return Err(ModelsResponseError::InvalidResponse);
        }

        let mut ids = BTreeSet::new();
        for entry in decoded.data {
            if entry.id.is_empty()
                || entry.id.len() > MAX_MODEL_ID_BYTES
                || entry.id.chars().any(char::is_whitespace)
                || entry.id.chars().any(char::is_control)
                || !ids.insert(entry.id)
            {
                return Err(ModelsResponseError::InvalidResponse);
            }
        }

        Ok(ids
            .into_iter()
            .map(|id| {
                let key = format!("{}/{id}", profile.template());
                let release_catalog_key =
                    ProviderCatalog::release().model(&key).and_then(|record| {
                        (record.provider_template() == profile.template()
                            && record.model_id().value() == id)
                            .then(|| record.key().to_owned())
                    });
                ObservedProviderModel {
                    id,
                    release_catalog_key,
                }
            })
            .collect())
    }
}

impl<V: CredentialVault> ProviderConnectionTester for ModelsHttpConnectionTester<'_, V> {
    fn test(&mut self, profile: &ProviderProfileSnapshot) -> ProviderConnectionTestStatus {
        let Some(endpoint) = profile.models_endpoint() else {
            return Self::failed(ProviderConnectionFailureCategory::MissingModelsRoute, false);
        };
        let endpoint =
            match validate_provider_endpoint(&endpoint, profile.allow_insecure_loopback()) {
                Ok(endpoint) => endpoint,
                Err(_) => {
                    return Self::failed(
                        ProviderConnectionFailureCategory::InvalidConfiguration,
                        false,
                    );
                }
            };
        let scope = match ProviderCredentialScope::from_profile(profile) {
            Ok(scope) => scope,
            Err(_) => {
                return Self::failed(
                    ProviderConnectionFailureCategory::InvalidConfiguration,
                    false,
                );
            }
        };
        let secret = match self.vault.resolve(&scope) {
            Ok(secret) => secret,
            Err(CredentialVaultError::NotFound) => {
                return Self::failed(ProviderConnectionFailureCategory::CredentialMissing, false);
            }
            Err(CredentialVaultError::Unavailable) => {
                return Self::failed(
                    ProviderConnectionFailureCategory::CredentialUnavailable,
                    true,
                );
            }
            Err(
                CredentialVaultError::InvalidScope(_)
                | CredentialVaultError::InvalidSecret
                | CredentialVaultError::AlreadyBound,
            ) => {
                return Self::failed(
                    ProviderConnectionFailureCategory::InvalidConfiguration,
                    false,
                );
            }
        };
        let authorization = match bearer_header(&secret) {
            Ok(authorization) => authorization,
            Err(_) => {
                return Self::failed(
                    ProviderConnectionFailureCategory::InvalidConfiguration,
                    false,
                );
            }
        };
        let client = match Client::builder()
            .no_proxy()
            .https_only(endpoint.scheme() == "https")
            .timeout(self.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(client) => client,
            Err(_) => {
                return Self::failed(ProviderConnectionFailureCategory::Unavailable, true);
            }
        };
        let response = match client
            .get(endpoint)
            .header(ACCEPT, "application/json")
            .header(AUTHORIZATION, authorization)
            .send()
        {
            Ok(response) => response,
            Err(_) => {
                return Self::failed(ProviderConnectionFailureCategory::Unavailable, true);
            }
        };
        let status = response.status();
        if status.is_success() {
            match Self::parse_models(profile, response) {
                Ok(models) => ProviderConnectionTestStatus::Succeeded {
                    profile: profile.profile().to_owned(),
                    fingerprint: profile.fingerprint(),
                    models,
                },
                Err(ModelsResponseError::Unavailable) => {
                    Self::failed(ProviderConnectionFailureCategory::Unavailable, true)
                }
                Err(ModelsResponseError::InvalidResponse) => {
                    Self::failed(ProviderConnectionFailureCategory::InvalidResponse, false)
                }
            }
        } else if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            Self::failed(ProviderConnectionFailureCategory::CredentialRejected, false)
        } else if matches!(
            status,
            StatusCode::REQUEST_TIMEOUT | StatusCode::TOO_MANY_REQUESTS
        ) || status.is_server_error()
        {
            Self::failed(ProviderConnectionFailureCategory::Unavailable, true)
        } else if status.is_redirection() {
            Self::failed(ProviderConnectionFailureCategory::InvalidResponse, false)
        } else if status.is_client_error() {
            Self::failed(ProviderConnectionFailureCategory::RequestRejected, false)
        } else {
            Self::failed(ProviderConnectionFailureCategory::InvalidResponse, false)
        }
    }
}
