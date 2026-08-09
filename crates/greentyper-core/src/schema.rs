//! Shared schema-version convention for persisted and exchanged data.

use std::error::Error;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct SchemaVersion(u16);

impl SchemaVersion {
    pub const fn new(version: u16) -> Result<Self, SchemaError> {
        if version == 0 {
            return Err(SchemaError::ZeroVersion);
        }
        Ok(Self(version))
    }

    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaKind {
    AcceptanceEvidence,
    BenchmarkEvidence,
    ConfigFile,
    ConfigEpoch,
    DeterministicFixture,
    LedgerFormat,
    RuntimeEvent,
    TeamEvent,
    ToolEvent,
}

impl SchemaKind {
    #[must_use]
    pub const fn current(self) -> SchemaVersion {
        match self {
            Self::AcceptanceEvidence
            | Self::ConfigFile
            | Self::ConfigEpoch
            | Self::DeterministicFixture
            | Self::LedgerFormat
            | Self::ToolEvent => SchemaVersion(1),
            Self::RuntimeEvent | Self::TeamEvent => SchemaVersion(2),
            Self::BenchmarkEvidence => SchemaVersion(2),
        }
    }

    pub fn require_current(self, actual: u16) -> Result<SchemaVersion, SchemaError> {
        let actual = SchemaVersion::new(actual)?;
        let supported = self.current();
        if actual.0 != supported.0 {
            return Err(SchemaError::Unsupported {
                kind: self,
                supported,
                actual,
            });
        }
        Ok(actual)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaError {
    ZeroVersion,
    Unsupported {
        kind: SchemaKind,
        supported: SchemaVersion,
        actual: SchemaVersion,
    },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroVersion => write!(formatter, "schema version zero is reserved"),
            Self::Unsupported {
                kind,
                supported,
                actual,
            } => write!(
                formatter,
                "unsupported {kind:?} schema version {}; expected {}",
                actual.get(),
                supported.get()
            ),
        }
    }
}

impl Error for SchemaError {}
