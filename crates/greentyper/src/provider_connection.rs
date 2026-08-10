//! Provider-neutral connection-test status with the current bounded HTTP models probe.

use std::time::Duration;

use greentyper_core::provider::ProviderProfileSnapshot;
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::header::{ACCEPT, AUTHORIZATION};
use serde::Serialize;

use crate::credential_vault::{CredentialVault, CredentialVaultError, ProviderCredentialScope};
use crate::provider_http_policy::{bearer_header, validate_provider_endpoint};

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
            ProviderConnectionTestStatus::Succeeded {
                profile: profile.profile().to_owned(),
                fingerprint: profile.fingerprint(),
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
