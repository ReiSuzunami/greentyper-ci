//! Shared HTTP credential and endpoint policy for concrete Provider adapters.

use std::net::IpAddr;

use greentyper_core::provider::ProviderError;
use reqwest::Url;
use reqwest::header::HeaderValue;

use crate::credential_vault::SecretValue;

pub(crate) fn bearer_header(secret: &SecretValue) -> Result<HeaderValue, ProviderError> {
    let mut bytes = Vec::with_capacity("Bearer ".len() + secret.expose().len());
    bytes.extend_from_slice(b"Bearer ");
    bytes.extend_from_slice(secret.expose());
    let header = HeaderValue::from_bytes(&bytes)
        .map_err(|_| ProviderError::InvalidConfiguration("Provider credential is invalid"));
    bytes.fill(0);
    let mut header = header?;
    header.set_sensitive(true);
    Ok(header)
}

pub(crate) fn validate_provider_endpoint(
    value: &str,
    allow_insecure_loopback: bool,
) -> Result<Url, ProviderError> {
    let endpoint = Url::parse(value).map_err(|_| {
        ProviderError::InvalidConfiguration("Provider endpoint must be an absolute URL")
    })?;
    if !matches!(endpoint.scheme(), "http" | "https")
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(ProviderError::InvalidConfiguration(
            "Provider endpoint contains unsupported URL components",
        ));
    }
    let host = endpoint
        .host_str()
        .ok_or(ProviderError::InvalidConfiguration(
            "Provider endpoint has no host",
        ))?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if endpoint.scheme() == "http" && (!loopback || !allow_insecure_loopback) {
        return Err(ProviderError::InvalidConfiguration(
            "plain HTTP requires explicit loopback permission",
        ));
    }
    if !loopback && allow_insecure_loopback {
        return Err(ProviderError::InvalidConfiguration(
            "loopback permission is invalid for a remote endpoint",
        ));
    }
    Ok(endpoint)
}
