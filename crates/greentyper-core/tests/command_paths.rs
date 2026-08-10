use std::collections::BTreeSet;

use greentyper_core::config::{
    CommandMatchKind, CommandQueryError, CommandTarget, ConfigReadback, ConfigSection,
    MAX_COMMAND_QUERY_BYTES, MAX_COMMAND_QUERY_TOKENS, command_paths, config_schema,
    match_command_paths, root_command_paths,
};

#[test]
fn root_panel_is_curated_and_nested_commands_stay_nested() {
    let canonical = root_command_paths()
        .iter()
        .map(|path| path.canonical())
        .collect::<Vec<_>>();
    assert_eq!(canonical, ["/config", "/model", "/stats", "/agent"]);
    assert!(
        command_paths()
            .iter()
            .filter(|path| !path.root_visible())
            .all(|path| path.canonical().split_whitespace().count() > 1)
    );

    let root = match_command_paths("/").expect("root command panel");
    assert_eq!(root.len(), 4);
    assert!(root.iter().all(|entry| entry.path().root_visible()));
}

#[test]
fn token_matching_opens_config_and_focuses_provider_url() {
    let config = match_command_paths("/con").expect("prefix config match");
    assert_eq!(config[0].path().canonical(), "/config");
    assert_eq!(config[0].kind(), CommandMatchKind::Prefix);
    assert_eq!(config[0].path().target(), CommandTarget::ConfigCenter);

    let url = match_command_paths("/config pro url").expect("provider URL match");
    assert_eq!(url[0].path().canonical(), "/config provider url");
    assert_eq!(url[0].kind(), CommandMatchKind::Prefix);
    assert_eq!(
        url[0].path().target(),
        CommandTarget::ConfigEditor {
            path_pattern: "providers.<id>.base_url",
            readback: ConfigReadback::Value,
        }
    );

    let fuzzy = match_command_paths("/cfg pro url").expect("fuzzy config match");
    assert_eq!(fuzzy[0].path().canonical(), "/config provider url");
    assert_eq!(fuzzy[0].kind(), CommandMatchKind::Fuzzy);

    let exact = match_command_paths("  /CONFIG PROVIDER URL  ").expect("exact normalized match");
    assert_eq!(exact[0].path().canonical(), "/config provider url");
    assert_eq!(exact[0].kind(), CommandMatchKind::Exact);
}

#[test]
fn root_actions_and_config_sections_remain_semantically_distinct() {
    let roots = [
        ("/config", CommandTarget::ConfigCenter),
        ("/model", CommandTarget::ModelSelector),
        ("/stats", CommandTarget::Stats),
        ("/agent", CommandTarget::AgentCenter),
    ];
    for (query, target) in roots {
        let matched = match_command_paths(query).expect("root match");
        assert_eq!(matched[0].path().target(), target);
        assert_eq!(matched[0].kind(), CommandMatchKind::Exact);
    }

    let sections = [
        ("provider", ConfigSection::Provider),
        ("model", ConfigSection::Model),
        ("pricing", ConfigSection::Pricing),
        ("statusline", ConfigSection::Statusline),
        ("stats-window", ConfigSection::StatsWindow),
        ("agent", ConfigSection::Agent),
        ("skills", ConfigSection::Skills),
        ("mcp", ConfigSection::Mcp),
        ("security", ConfigSection::Security),
    ];
    for (token, section) in sections {
        let query = format!("/config {token}");
        let matched = match_command_paths(&query).expect("section match");
        assert_eq!(
            matched[0].path().target(),
            CommandTarget::ConfigSection { section }
        );
        assert_eq!(matched[0].kind(), CommandMatchKind::Exact);
    }
}

#[test]
fn config_object_lifecycle_actions_remain_nested_and_typed() {
    let actions = [
        (
            "/config provider add",
            CommandTarget::ConfigObjectCreate {
                kind: greentyper_core::config::ConfigObjectKind::ProviderProfile,
            },
        ),
        (
            "/config provider remove",
            CommandTarget::ConfigObjectDelete {
                kind: greentyper_core::config::ConfigObjectKind::ProviderProfile,
            },
        ),
        (
            "/config model add",
            CommandTarget::ConfigObjectCreate {
                kind: greentyper_core::config::ConfigObjectKind::ModelPreset,
            },
        ),
        (
            "/config model remove",
            CommandTarget::ConfigObjectDelete {
                kind: greentyper_core::config::ConfigObjectKind::ModelPreset,
            },
        ),
        (
            "/config pricing add",
            CommandTarget::ConfigObjectCreate {
                kind: greentyper_core::config::ConfigObjectKind::PriceSchedule,
            },
        ),
        (
            "/config pricing remove",
            CommandTarget::ConfigObjectDelete {
                kind: greentyper_core::config::ConfigObjectKind::PriceSchedule,
            },
        ),
        (
            "/config stats-window add",
            CommandTarget::ConfigObjectCreate {
                kind: greentyper_core::config::ConfigObjectKind::UsageWindow,
            },
        ),
        (
            "/config stats-window remove",
            CommandTarget::ConfigObjectDelete {
                kind: greentyper_core::config::ConfigObjectKind::UsageWindow,
            },
        ),
    ];

    for (query, target) in actions {
        let matched = match_command_paths(query).expect("lifecycle action match");
        assert_eq!(matched[0].path().canonical(), query);
        assert_eq!(matched[0].path().target(), target);
        assert_eq!(matched[0].kind(), CommandMatchKind::Exact);
        assert!(!matched[0].path().root_visible());
    }
}

#[test]
fn every_config_schema_field_has_exactly_one_editor_route() {
    let routed = command_paths()
        .iter()
        .filter_map(|path| match path.target() {
            CommandTarget::ConfigEditor { path_pattern, .. } => Some(path_pattern),
            _ => None,
        })
        .collect::<Vec<_>>();
    let unique = routed.iter().copied().collect::<BTreeSet<_>>();
    let expected = config_schema()
        .iter()
        .map(|entry| entry.path_pattern)
        .collect::<BTreeSet<_>>();

    assert_eq!(routed.len(), unique.len(), "duplicate editor route");
    assert_eq!(unique, expected, "schema field without an editor route");
}

#[test]
fn credential_editor_never_offers_value_readback() {
    let credential = command_paths()
        .iter()
        .find(|path| path.canonical() == "/config provider credential")
        .expect("credential command route");
    assert_eq!(
        credential.target(),
        CommandTarget::ConfigEditor {
            path_pattern: "providers.<id>.credential",
            readback: ConfigReadback::BindingStatusOnly,
        }
    );
}

#[test]
fn command_query_is_bounded_and_rejects_invalid_shapes() {
    assert!(match_command_paths("config").is_err());
    assert!(match_command_paths("//config").is_err());
    assert!(match_command_paths("/config provider $").is_err());
    assert!(match_command_paths(&format!("/{}", "a".repeat(257))).is_err());
    assert_eq!(
        match_command_paths(&format!("{} /config", " ".repeat(257))),
        Err(CommandQueryError::TooLong),
        "the byte limit applies to the raw untrusted query"
    );
    assert!(
        match_command_paths("/not-a-command")
            .expect("valid query")
            .is_empty()
    );

    let max_bytes = format!("/{}", "a".repeat(MAX_COMMAND_QUERY_BYTES - 1));
    assert!(
        match_command_paths(&max_bytes)
            .expect("maximum byte query")
            .is_empty()
    );
    let max_tokens = format!("/{}", vec!["a"; MAX_COMMAND_QUERY_TOKENS].join(" "));
    assert!(
        match_command_paths(&max_tokens)
            .expect("maximum token query")
            .is_empty()
    );
    let too_many_tokens = format!("/{}", vec!["a"; MAX_COMMAND_QUERY_TOKENS + 1].join(" "));
    assert_eq!(
        match_command_paths(&too_many_tokens),
        Err(CommandQueryError::TooManyTokens)
    );
}

#[test]
fn canonical_command_paths_are_unique() {
    let all = command_paths()
        .iter()
        .map(|path| path.canonical())
        .collect::<Vec<_>>();
    let unique = all.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(all.len(), unique.len());
}
