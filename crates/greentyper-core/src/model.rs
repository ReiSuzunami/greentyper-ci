//! Provider-neutral identifiers and canonical conversation items.

use std::error::Error;
use std::fmt;

pub const MAX_ITEM_TEXT_BYTES: usize = 1024 * 1024;

macro_rules! nonzero_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub const fn new(value: u64) -> Result<Self, ModelError> {
                if value == 0 {
                    return Err(ModelError::ZeroId(stringify!($name)));
                }
                Ok(Self(value))
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

nonzero_id!(ThreadId);
nonzero_id!(TurnId);
nonzero_id!(ItemId);
nonzero_id!(DeliveryId);
nonzero_id!(ConfigEpochId);
nonzero_id!(ProviderEpochId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ItemRole {
    User,
    Assistant,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalItem {
    id: ItemId,
    turn: TurnId,
    role: ItemRole,
    text: String,
}

impl CanonicalItem {
    pub fn new(
        id: ItemId,
        turn: TurnId,
        role: ItemRole,
        text: impl Into<String>,
    ) -> Result<Self, ModelError> {
        let text = text.into();
        if text.trim().is_empty() {
            return Err(ModelError::EmptyItem);
        }
        if text.len() > MAX_ITEM_TEXT_BYTES {
            return Err(ModelError::ItemTooLarge);
        }
        Ok(Self {
            id,
            turn,
            role,
            text,
        })
    }

    #[must_use]
    pub const fn id(&self) -> ItemId {
        self.id
    }

    #[must_use]
    pub const fn turn(&self) -> TurnId {
        self.turn
    }

    #[must_use]
    pub const fn role(&self) -> ItemRole {
        self.role
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModelError {
    ZeroId(&'static str),
    EmptyItem,
    ItemTooLarge,
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroId(kind) => write!(formatter, "{kind} zero is reserved"),
            Self::EmptyItem => write!(formatter, "canonical item text cannot be empty"),
            Self::ItemTooLarge => write!(formatter, "canonical item text is too large"),
        }
    }
}

impl Error for ModelError {}
