use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::net::IpAddr;

use greentyper_core::provider::ProviderProfileSnapshot;
use reqwest::Url;

const MAX_REFERENCE_BYTES: usize = 64;
pub(crate) const MAX_SECRET_BYTES: usize = 2560;

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct ProviderCredentialScope {
    target_name: String,
}

impl ProviderCredentialScope {
    pub(crate) fn from_profile(
        profile: &ProviderProfileSnapshot,
    ) -> Result<Self, CredentialVaultError> {
        let reference =
            profile
                .credential_reference()
                .ok_or(CredentialVaultError::InvalidScope(
                    "Provider Profile has no credential reference",
                ))?;
        let base_url = profile
            .base_url()
            .ok_or(CredentialVaultError::InvalidScope(
                "Provider Profile has no origin",
            ))?;
        Self::new(
            profile.profile(),
            reference,
            base_url,
            profile.allow_insecure_loopback(),
        )
    }

    pub(crate) fn new(
        profile: &str,
        reference: &str,
        base_url: &str,
        allow_insecure_loopback: bool,
    ) -> Result<Self, CredentialVaultError> {
        validate_reference(profile)?;
        validate_reference(reference)?;
        let url = Url::parse(base_url)
            .map_err(|_| CredentialVaultError::InvalidScope("provider origin is invalid"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(CredentialVaultError::InvalidScope(
                "provider origin contains unsupported components",
            ));
        }
        let host = url.host_str().ok_or(CredentialVaultError::InvalidScope(
            "provider origin has no host",
        ))?;
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<IpAddr>()
                .is_ok_and(|address| address.is_loopback());
        if url.scheme() == "http" && (!loopback || !allow_insecure_loopback) {
            return Err(CredentialVaultError::InvalidScope(
                "plain HTTP requires explicit loopback permission",
            ));
        }
        if !loopback && allow_insecure_loopback {
            return Err(CredentialVaultError::InvalidScope(
                "loopback permission is invalid for a remote origin",
            ));
        }

        let origin = url.origin().ascii_serialization();
        Ok(Self {
            target_name: format!("GreenTyper/provider/{profile}/{reference}/{origin}"),
        })
    }

    pub(crate) fn target_name(&self) -> &str {
        &self.target_name
    }
}

impl fmt::Debug for ProviderCredentialScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCredentialScope")
            .field("origin_bound", &true)
            .finish()
    }
}

pub(crate) struct SecretValue {
    bytes: Vec<u8>,
}

impl SecretValue {
    pub(crate) fn new(bytes: Vec<u8>) -> Result<Self, CredentialVaultError> {
        if bytes.is_empty()
            || bytes.len() > MAX_SECRET_BYTES
            || bytes.iter().any(u8::is_ascii_control)
        {
            return Err(CredentialVaultError::InvalidSecret);
        }
        Ok(Self { bytes })
    }

    pub(crate) fn expose(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretValue")
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl PartialEq for SecretValue {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for SecretValue {}

impl Drop for SecretValue {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

pub(crate) trait CredentialVault {
    fn bind(
        &mut self,
        scope: &ProviderCredentialScope,
        secret: SecretValue,
    ) -> Result<(), CredentialVaultError>;

    fn replace(
        &mut self,
        scope: &ProviderCredentialScope,
        secret: SecretValue,
    ) -> Result<(), CredentialVaultError>;

    fn resolve(&self, scope: &ProviderCredentialScope)
    -> Result<SecretValue, CredentialVaultError>;

    fn forget(&mut self, scope: &ProviderCredentialScope) -> Result<bool, CredentialVaultError>;
}

#[derive(Default)]
pub(crate) struct InMemoryCredentialVault {
    entries: BTreeMap<String, SecretValue>,
}

impl CredentialVault for InMemoryCredentialVault {
    fn bind(
        &mut self,
        scope: &ProviderCredentialScope,
        secret: SecretValue,
    ) -> Result<(), CredentialVaultError> {
        if self.entries.contains_key(scope.target_name()) {
            return Err(CredentialVaultError::AlreadyBound);
        }
        self.entries.insert(scope.target_name().to_owned(), secret);
        Ok(())
    }

    fn replace(
        &mut self,
        scope: &ProviderCredentialScope,
        secret: SecretValue,
    ) -> Result<(), CredentialVaultError> {
        let entry = self
            .entries
            .get_mut(scope.target_name())
            .ok_or(CredentialVaultError::NotFound)?;
        *entry = secret;
        Ok(())
    }

    fn resolve(
        &self,
        scope: &ProviderCredentialScope,
    ) -> Result<SecretValue, CredentialVaultError> {
        let entry = self
            .entries
            .get(scope.target_name())
            .ok_or(CredentialVaultError::NotFound)?;
        SecretValue::new(entry.expose().to_vec())
    }

    fn forget(&mut self, scope: &ProviderCredentialScope) -> Result<bool, CredentialVaultError> {
        Ok(self.entries.remove(scope.target_name()).is_some())
    }
}

#[derive(Default)]
pub(crate) struct PlatformCredentialVault;

#[cfg(not(windows))]
impl CredentialVault for PlatformCredentialVault {
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
        _scope: &ProviderCredentialScope,
    ) -> Result<SecretValue, CredentialVaultError> {
        Err(CredentialVaultError::Unavailable)
    }

    fn forget(&mut self, _scope: &ProviderCredentialScope) -> Result<bool, CredentialVaultError> {
        Err(CredentialVaultError::Unavailable)
    }
}

#[cfg(windows)]
mod windows {
    #![allow(unsafe_code)]

    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
    use windows_sys::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree,
        CredReadW, CredWriteW,
    };

    use super::{
        CredentialVault, CredentialVaultError, PlatformCredentialVault, ProviderCredentialScope,
        SecretValue,
    };

    impl CredentialVault for PlatformCredentialVault {
        fn bind(
            &mut self,
            scope: &ProviderCredentialScope,
            secret: SecretValue,
        ) -> Result<(), CredentialVaultError> {
            match read(scope) {
                Ok(_) => Err(CredentialVaultError::AlreadyBound),
                Err(CredentialVaultError::NotFound) => write(scope, &secret),
                Err(error) => Err(error),
            }
        }

        fn replace(
            &mut self,
            scope: &ProviderCredentialScope,
            secret: SecretValue,
        ) -> Result<(), CredentialVaultError> {
            read(scope)?;
            write(scope, &secret)
        }

        fn resolve(
            &self,
            scope: &ProviderCredentialScope,
        ) -> Result<SecretValue, CredentialVaultError> {
            read(scope)
        }

        fn forget(
            &mut self,
            scope: &ProviderCredentialScope,
        ) -> Result<bool, CredentialVaultError> {
            let target = wide(scope.target_name());
            if unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) } != 0 {
                return Ok(true);
            }
            if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                Ok(false)
            } else {
                Err(CredentialVaultError::Unavailable)
            }
        }
    }

    fn read(scope: &ProviderCredentialScope) -> Result<SecretValue, CredentialVaultError> {
        let target = wide(scope.target_name());
        let mut credential = ptr::null_mut();
        if unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut credential) } == 0 {
            return if unsafe { GetLastError() } == ERROR_NOT_FOUND {
                Err(CredentialVaultError::NotFound)
            } else {
                Err(CredentialVaultError::Unavailable)
            };
        }
        if credential.is_null() {
            return Err(CredentialVaultError::Unavailable);
        }
        let allocation = CredentialAllocation(credential);
        let credential = unsafe { &*allocation.0 };
        let size = usize::try_from(credential.CredentialBlobSize)
            .map_err(|_| CredentialVaultError::InvalidSecret)?;
        if credential.CredentialBlob.is_null() {
            return Err(CredentialVaultError::InvalidSecret);
        }
        let bytes = unsafe { std::slice::from_raw_parts(credential.CredentialBlob, size) };
        SecretValue::new(bytes.to_vec())
    }

    fn write(
        scope: &ProviderCredentialScope,
        secret: &SecretValue,
    ) -> Result<(), CredentialVaultError> {
        let mut target = wide(scope.target_name());
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_mut_ptr(),
            CredentialBlobSize: u32::try_from(secret.expose().len())
                .map_err(|_| CredentialVaultError::InvalidSecret)?,
            CredentialBlob: secret.expose().as_ptr().cast_mut(),
            // Persists across logon sessions for this user, never across users.
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..CREDENTIALW::default()
        };
        if unsafe { CredWriteW(&credential, 0) } == 0 {
            Err(CredentialVaultError::Unavailable)
        } else {
            Ok(())
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        std::ffi::OsStr::new(value)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    struct CredentialAllocation(*mut CREDENTIALW);

    impl Drop for CredentialAllocation {
        fn drop(&mut self) {
            unsafe { CredFree(self.0.cast::<c_void>()) };
        }
    }
}

fn validate_reference(value: &str) -> Result<(), CredentialVaultError> {
    if value.is_empty()
        || value.len() > MAX_REFERENCE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        Err(CredentialVaultError::InvalidScope(
            "credential scope IDs must be lowercase identifiers",
        ))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CredentialVaultError {
    InvalidScope(&'static str),
    InvalidSecret,
    AlreadyBound,
    NotFound,
    Unavailable,
}

impl fmt::Display for CredentialVaultError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidScope(reason) => {
                write!(formatter, "credential scope is invalid: {reason}")
            }
            Self::InvalidSecret => formatter.write_str("credential value is invalid"),
            Self::AlreadyBound => formatter.write_str("credential reference is already bound"),
            Self::NotFound => formatter.write_str("credential reference was not found"),
            Self::Unavailable => formatter.write_str("platform credential vault is unavailable"),
        }
    }
}

impl Error for CredentialVaultError {}

#[cfg(test)]
mod tests {
    use super::{
        CredentialVault, CredentialVaultError, InMemoryCredentialVault, PlatformCredentialVault,
        ProviderCredentialScope, SecretValue,
    };

    #[test]
    fn credential_scope_is_bound_to_profile_reference_and_origin() {
        let first = ProviderCredentialScope::new(
            "openai-main",
            "openai-main",
            "https://gateway.example.com/v1",
            false,
        )
        .expect("first credential scope");
        let same_origin = ProviderCredentialScope::new(
            "openai-main",
            "openai-main",
            "https://gateway.example.com/other",
            false,
        )
        .expect("same-origin credential scope");
        let other_origin = ProviderCredentialScope::new(
            "openai-main",
            "openai-main",
            "https://gateway.example.com:8443/v1",
            false,
        )
        .expect("other-origin credential scope");

        assert_eq!(first, same_origin);
        assert_ne!(first, other_origin);

        let debug = format!("{first:?}");
        assert!(!debug.contains("openai-main"));
        assert!(!debug.contains("gateway.example.com"));
        assert!(debug.contains("origin_bound"));
    }

    #[test]
    fn vault_operations_never_cross_origin_or_reveal_secret_debug() {
        let scope = ProviderCredentialScope::new(
            "openai-main",
            "openai-main",
            "https://gateway.example.com/v1",
            false,
        )
        .expect("credential scope");
        let other_origin = ProviderCredentialScope::new(
            "openai-main",
            "openai-main",
            "https://other.example.com/v1",
            false,
        )
        .expect("other-origin credential scope");
        let mut vault = InMemoryCredentialVault::default();
        let first = SecretValue::new(b"private-first-token".to_vec()).expect("first secret");
        assert!(!format!("{first:?}").contains("private-first-token"));

        vault.bind(&scope, first).expect("bind credential");
        assert_eq!(
            vault.bind(
                &scope,
                SecretValue::new(b"private-overwrite-token".to_vec()).unwrap()
            ),
            Err(CredentialVaultError::AlreadyBound)
        );
        assert_eq!(
            vault.resolve(&other_origin),
            Err(CredentialVaultError::NotFound)
        );
        assert_eq!(
            vault.resolve(&scope).unwrap().expose(),
            b"private-first-token"
        );

        vault
            .replace(
                &scope,
                SecretValue::new(b"private-second-token".to_vec()).unwrap(),
            )
            .expect("replace credential");
        assert_eq!(
            vault.resolve(&scope).unwrap().expose(),
            b"private-second-token"
        );
        assert!(vault.forget(&scope).expect("forget credential"));
        assert!(!vault.forget(&scope).expect("forget missing credential"));
        assert_eq!(vault.resolve(&scope), Err(CredentialVaultError::NotFound));
    }

    #[cfg(not(windows))]
    #[test]
    fn platform_vault_fails_closed_outside_windows() {
        let scope = ProviderCredentialScope::new(
            "openai-main",
            "openai-main",
            "https://gateway.example.com/v1",
            false,
        )
        .expect("credential scope");
        let mut vault = PlatformCredentialVault;

        assert_eq!(
            vault.bind(
                &scope,
                SecretValue::new(b"private-platform-token".to_vec()).unwrap()
            ),
            Err(CredentialVaultError::Unavailable)
        );
        assert_eq!(
            vault.resolve(&scope),
            Err(CredentialVaultError::Unavailable)
        );
        assert_eq!(vault.forget(&scope), Err(CredentialVaultError::Unavailable));
    }

    #[cfg(windows)]
    #[test]
    fn windows_credential_manager_round_trips_and_forgets_secret() {
        let reference = format!("ci-{}", std::process::id());
        let scope = ProviderCredentialScope::new(
            "ci-provider",
            &reference,
            "https://credential-test.invalid/v1",
            false,
        )
        .expect("Windows credential scope");
        let mut vault = PlatformCredentialVault;
        let _ = vault.forget(&scope);
        let _cleanup = WindowsCredentialCleanup(scope.clone());

        vault
            .bind(
                &scope,
                SecretValue::new(b"synthetic-windows-first".to_vec()).unwrap(),
            )
            .expect("bind Windows credential");
        assert_eq!(
            vault.resolve(&scope).unwrap().expose(),
            b"synthetic-windows-first"
        );
        vault
            .replace(
                &scope,
                SecretValue::new(b"synthetic-windows-second".to_vec()).unwrap(),
            )
            .expect("replace Windows credential");
        assert_eq!(
            vault.resolve(&scope).unwrap().expose(),
            b"synthetic-windows-second"
        );
        assert!(vault.forget(&scope).expect("forget Windows credential"));
        assert_eq!(vault.resolve(&scope), Err(CredentialVaultError::NotFound));
    }

    #[cfg(windows)]
    struct WindowsCredentialCleanup(ProviderCredentialScope);

    #[cfg(windows)]
    impl Drop for WindowsCredentialCleanup {
        fn drop(&mut self) {
            let _ = PlatformCredentialVault.forget(&self.0);
        }
    }
}
