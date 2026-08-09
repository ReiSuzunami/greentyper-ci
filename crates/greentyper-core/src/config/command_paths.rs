//! Hierarchical command paths derived from the Config schema contract.

use std::error::Error;
use std::fmt;
use std::sync::LazyLock;

use serde::Serialize;

use super::runtime::{ConfigObjectKind, config_schema};

pub const MAX_COMMAND_QUERY_BYTES: usize = 256;
pub const MAX_COMMAND_QUERY_TOKENS: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigReadback {
    Value,
    BindingStatusOnly,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSection {
    Provider,
    Model,
    Statusline,
    StatsWindow,
    Agent,
    Skills,
    Mcp,
    Security,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CommandTarget {
    ConfigCenter,
    ConfigSection {
        section: ConfigSection,
    },
    ConfigObjectCreate {
        kind: ConfigObjectKind,
    },
    ConfigObjectDelete {
        kind: ConfigObjectKind,
    },
    ConfigEditor {
        path_pattern: &'static str,
        readback: ConfigReadback,
    },
    ModelSelector,
    Stats,
    AgentCenter,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CommandPath {
    canonical: &'static str,
    target: CommandTarget,
    root_visible: bool,
}

impl CommandPath {
    #[must_use]
    pub const fn canonical(&self) -> &'static str {
        self.canonical
    }

    #[must_use]
    pub const fn target(&self) -> CommandTarget {
        self.target
    }

    #[must_use]
    pub const fn root_visible(&self) -> bool {
        self.root_visible
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandMatchKind {
    Exact,
    Prefix,
    Fuzzy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandMatch {
    path: &'static CommandPath,
    kind: CommandMatchKind,
    score: u16,
    remaining_tokens: usize,
}

impl CommandMatch {
    #[must_use]
    pub const fn path(&self) -> &'static CommandPath {
        self.path
    }

    #[must_use]
    pub const fn kind(&self) -> CommandMatchKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandQueryError {
    MissingSlash,
    TooLong,
    TooManyTokens,
    InvalidToken,
}

impl fmt::Display for CommandQueryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::MissingSlash => "command query must begin with '/'",
            Self::TooLong => "command query is too long",
            Self::TooManyTokens => "command query has too many tokens",
            Self::InvalidToken => "command query contains an invalid token",
        })
    }
}

impl Error for CommandQueryError {}

const fn root(canonical: &'static str, target: CommandTarget) -> CommandPath {
    CommandPath {
        canonical,
        target,
        root_visible: true,
    }
}

const fn nested(canonical: &'static str, target: CommandTarget) -> CommandPath {
    CommandPath {
        canonical,
        target,
        root_visible: false,
    }
}

const fn section(canonical: &'static str, section: ConfigSection) -> CommandPath {
    nested(canonical, CommandTarget::ConfigSection { section })
}

const ROOT_COMMAND_COUNT: usize = 4;

const NAVIGATION_PATHS: &[CommandPath] = &[
    root("/config", CommandTarget::ConfigCenter),
    root("/model", CommandTarget::ModelSelector),
    root("/stats", CommandTarget::Stats),
    root("/agent", CommandTarget::AgentCenter),
    section("/config provider", ConfigSection::Provider),
    nested(
        "/config provider add",
        CommandTarget::ConfigObjectCreate {
            kind: ConfigObjectKind::ProviderProfile,
        },
    ),
    nested(
        "/config provider remove",
        CommandTarget::ConfigObjectDelete {
            kind: ConfigObjectKind::ProviderProfile,
        },
    ),
    section("/config model", ConfigSection::Model),
    nested(
        "/config model add",
        CommandTarget::ConfigObjectCreate {
            kind: ConfigObjectKind::ModelPreset,
        },
    ),
    nested(
        "/config model remove",
        CommandTarget::ConfigObjectDelete {
            kind: ConfigObjectKind::ModelPreset,
        },
    ),
    section("/config statusline", ConfigSection::Statusline),
    section("/config stats-window", ConfigSection::StatsWindow),
    nested(
        "/config stats-window add",
        CommandTarget::ConfigObjectCreate {
            kind: ConfigObjectKind::UsageWindow,
        },
    ),
    nested(
        "/config stats-window remove",
        CommandTarget::ConfigObjectDelete {
            kind: ConfigObjectKind::UsageWindow,
        },
    ),
    section("/config agent", ConfigSection::Agent),
    section("/config skills", ConfigSection::Skills),
    section("/config mcp", ConfigSection::Mcp),
    section("/config security", ConfigSection::Security),
];

static COMMAND_PATHS: LazyLock<Vec<CommandPath>> = LazyLock::new(|| {
    let mut paths = NAVIGATION_PATHS.to_vec();
    paths.extend(config_schema().iter().map(|entry| {
        nested(
            entry.command_path,
            CommandTarget::ConfigEditor {
                path_pattern: entry.path_pattern,
                readback: if entry.credential_reference {
                    ConfigReadback::BindingStatusOnly
                } else {
                    ConfigReadback::Value
                },
            },
        )
    }));
    paths
});

#[must_use]
pub fn command_paths() -> &'static [CommandPath] {
    COMMAND_PATHS.as_slice()
}

#[must_use]
pub fn root_command_paths() -> &'static [CommandPath] {
    &command_paths()[..ROOT_COMMAND_COUNT]
}

pub fn match_command_paths(query: &str) -> Result<Vec<CommandMatch>, CommandQueryError> {
    if query.len() > MAX_COMMAND_QUERY_BYTES {
        return Err(CommandQueryError::TooLong);
    }
    let query = query.trim();
    let Some(body) = query.strip_prefix('/') else {
        return Err(CommandQueryError::MissingSlash);
    };
    if body.is_empty() {
        return Ok(root_command_paths()
            .iter()
            .map(|path| CommandMatch {
                path,
                kind: CommandMatchKind::Exact,
                score: 0,
                remaining_tokens: 0,
            })
            .collect());
    }

    let tokens = body.split_whitespace().collect::<Vec<_>>();
    if tokens.len() > MAX_COMMAND_QUERY_TOKENS {
        return Err(CommandQueryError::TooManyTokens);
    }
    if tokens.iter().any(|token| !valid_query_token(token)) {
        return Err(CommandQueryError::InvalidToken);
    }
    let tokens = tokens
        .into_iter()
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();

    let mut matches = command_paths()
        .iter()
        .filter_map(|path| match_path(path, &tokens))
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        left.score
            .cmp(&right.score)
            .then_with(|| left.remaining_tokens.cmp(&right.remaining_tokens))
            .then_with(|| left.path.canonical.cmp(right.path.canonical))
    });
    Ok(matches)
}

fn valid_query_token(token: &str) -> bool {
    !token.is_empty()
        && token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn match_path(path: &'static CommandPath, query: &[String]) -> Option<CommandMatch> {
    let candidate = path.canonical[1..].split_whitespace().collect::<Vec<_>>();
    if query.len() > candidate.len() {
        return None;
    }

    let mut kind = CommandMatchKind::Exact;
    let mut score = 0_u16;
    for (query, candidate) in query.iter().zip(candidate.iter()) {
        let token_kind = match_token(query, candidate)?;
        kind = kind.max(token_kind);
        score = score.checked_add(match token_kind {
            CommandMatchKind::Exact => 0,
            CommandMatchKind::Prefix => 1,
            CommandMatchKind::Fuzzy => 2,
        })?;
    }
    let remaining_tokens = candidate.len() - query.len();
    score = score.checked_add(u16::try_from(remaining_tokens).ok()?)?;
    Some(CommandMatch {
        path,
        kind,
        score,
        remaining_tokens,
    })
}

fn match_token(query: &str, candidate: &str) -> Option<CommandMatchKind> {
    if query == candidate {
        Some(CommandMatchKind::Exact)
    } else if candidate.starts_with(query) {
        Some(CommandMatchKind::Prefix)
    } else if is_subsequence(query, candidate) {
        Some(CommandMatchKind::Fuzzy)
    } else {
        None
    }
}

fn is_subsequence(query: &str, candidate: &str) -> bool {
    let mut query = query.bytes();
    let mut next = query.next();
    for candidate in candidate.bytes() {
        if next == Some(candidate) {
            next = query.next();
        }
    }
    next.is_none()
}
