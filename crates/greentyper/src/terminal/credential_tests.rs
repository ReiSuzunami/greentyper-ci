    #[cfg(windows)]
    use crate::credential_vault::PlatformCredentialVault;

    #[derive(Default)]
    struct CountingCredentialVault {
        calls: Cell<usize>,
    }

    impl CredentialVault for CountingCredentialVault {
        fn bind(
            &mut self,
            _scope: &ProviderCredentialScope,
            _secret: SecretValue,
        ) -> Result<(), CredentialVaultError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Ok(())
        }

        fn replace(
            &mut self,
            _scope: &ProviderCredentialScope,
            _secret: SecretValue,
        ) -> Result<(), CredentialVaultError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Ok(())
        }

        fn resolve(
            &self,
            _scope: &ProviderCredentialScope,
        ) -> Result<SecretValue, CredentialVaultError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Err(CredentialVaultError::NotFound)
        }

        fn forget(
            &mut self,
            _scope: &ProviderCredentialScope,
        ) -> Result<bool, CredentialVaultError> {
            self.calls.set(self.calls.get().saturating_add(1));
            Ok(false)
        }
    }

    #[test]
    fn terminal_loop_binds_provider_credential_without_readback_or_state_writes() {
        let root = terminal_test_root("provider-credential-bind-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let before = std::fs::read(paths.user()).expect("read provider config before bind");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let secret = "private-terminal-bind-token";
        let mut events: VecDeque<_> = "config provider credential"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        events.extend(secret.chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let mut tester = RecordingConnectionTester { calls: Vec::new() };
        let mut vault = InMemoryCredentialVault::default();

        let output = run_terminal_loop_with_credential_vault(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(80, 24).expect("viewport"),
            },
            &mut tester,
            &mut vault,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        let profile = config
            .provider_profile("edge")
            .expect("resolve Provider Profile")
            .expect("external Provider Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let stored = vault.resolve(&scope).expect("bound credential");
        assert_eq!(stored.expose(), secret.as_bytes());
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("Bind credential"));
        assert!(output.contains("hidden"));
        assert!(output.contains("Credential bound"));
        assert!(!output.contains(secret));
        assert!(!output.contains("synthetic-edge-credential-reference"));
        assert!(tester.calls.is_empty());
        assert_eq!(
            std::fs::read(paths.user()).expect("read provider config after bind"),
            before
        );
        assert!(!paths.project().exists());
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_confirms_and_replaces_provider_credential_without_readback() {
        let root = terminal_test_root("provider-credential-replace-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let before = std::fs::read(paths.user()).expect("read provider config before replace");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let profile = config
            .provider_profile("edge")
            .expect("resolve Provider Profile")
            .expect("external Provider Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let old_secret = "private-terminal-old-token";
        let new_secret = "private-terminal-new-token";
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(
                &scope,
                SecretValue::new(old_secret.as_bytes().to_vec()).expect("old secret"),
            )
            .expect("seed bound credential");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let mut events: VecDeque<_> = "config provider credential"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        events.extend(new_secret.chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let mut tester = RecordingConnectionTester { calls: Vec::new() };

        let output = run_terminal_loop_with_credential_vault(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(80, 24).expect("viewport"),
            },
            &mut tester,
            &mut vault,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        let stored = vault.resolve(&scope).expect("replaced credential");
        assert_eq!(stored.expose(), new_secret.as_bytes());
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("Confirm credential replacement"));
        assert!(!output.contains(old_secret));
        assert!(!output.contains(new_secret));
        assert!(!output.contains("synthetic-edge-credential-reference"));
        assert!(tester.calls.is_empty());
        assert_eq!(
            std::fs::read(paths.user()).expect("read provider config after replace"),
            before
        );
        assert!(!paths.project().exists());
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_tests_and_confirms_forget_for_exact_provider_credential() {
        let root = terminal_test_root("provider-credential-test-forget-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let before = std::fs::read(paths.user()).expect("read provider config before forget");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let profile = config
            .provider_profile("edge")
            .expect("resolve Provider Profile")
            .expect("external Provider Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let secret = "private-terminal-forget-token";
        let mut vault = InMemoryCredentialVault::default();
        vault
            .bind(
                &scope,
                SecretValue::new(secret.as_bytes().to_vec()).expect("secret"),
            )
            .expect("seed bound credential");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let mut events: VecDeque<_> = "config provider credential"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);
        let mut tester = RecordingConnectionTester { calls: Vec::new() };

        let output = run_terminal_loop_with_credential_vault(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(80, 24).expect("viewport"),
            },
            &mut tester,
            &mut vault,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        assert_eq!(
            vault.resolve(&scope),
            Err(crate::credential_vault::CredentialVaultError::NotFound)
        );
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("Credential available"));
        assert!(output.contains("Credential not found"));
        assert!(output.contains("Confirm credential removal"));
        assert!(!output.contains(secret));
        assert!(!output.contains("synthetic-edge-credential-reference"));
        assert!(tester.calls.is_empty());
        assert_eq!(
            std::fs::read(paths.user()).expect("read provider config after forget"),
            before
        );
        assert!(!paths.project().exists());
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_loop_binds_then_tests_the_same_origin_bound_credential() {
        let root = terminal_test_root("provider-credential-bind-test-loop");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let before = std::fs::read(paths.user()).expect("read provider config before flow");
        std::fs::write(paths.project(), "schema_version = 1\n")
            .expect("write existing project config");
        let project_before =
            std::fs::read(paths.project()).expect("read project config before flow");
        drop(RuntimeKernel::open(&ledger).expect("create existing Runtime Ledger"));
        let ledger_before = std::fs::read(&ledger).expect("read Runtime Ledger before flow");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let profile = config
            .provider_profile("edge")
            .expect("resolve Provider Profile")
            .expect("external Provider Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let secret = "private-terminal-provider-test-token";
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let shared = SharedCredentialVault::default();
        let mut credential_vault = shared.clone();
        let mut tester = VaultConnectionTester {
            vault: shared.clone(),
            calls: 0,
        };
        let mut events: VecDeque<_> = "config provider credential"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        events.extend(secret.chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop_with_credential_vault(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            TerminalSnapshotSource {
                initial: &view,
                refresh_ledger: Some(&ledger),
                viewport: Viewport::new(80, 24).expect("viewport"),
            },
            &mut tester,
            &mut credential_vault,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        let stored = shared.resolve(&scope).expect("bound credential");
        assert_eq!(stored.expose(), secret.as_bytes());
        assert_eq!(tester.calls, 1);
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("succeeded"));
        assert!(!output.contains(secret));
        assert!(!output.contains("synthetic-edge-credential-reference"));
        assert_eq!(
            std::fs::read(paths.user()).expect("read provider config after flow"),
            before
        );
        assert_eq!(
            std::fs::read(paths.project()).expect("read project config after flow"),
            project_before
        );
        assert_eq!(
            std::fs::read(&ledger).expect("read Runtime Ledger after flow"),
            ledger_before
        );
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_credential_secret_input_uses_one_bounded_allocation() {
        let mut input = super::CredentialSecretInput::default();
        let allocation = input.bytes.as_ptr();
        for _ in 0..super::MAX_SECRET_BYTES {
            assert!(input.push('x'));
            assert_eq!(input.bytes.as_ptr(), allocation);
        }
        assert!(!input.push('x'));
        input.clear();
        assert_eq!(input.bytes.as_ptr(), allocation);
    }

    #[test]
    fn terminal_rejects_credential_test_when_the_provider_scope_becomes_stale() {
        let root = terminal_test_root("provider-credential-stale-test-scope");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let mut session = TerminalSession::new("/config provider credential", 80, 24)
            .expect("terminal session");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Provider selector");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open credential field");
        session
            .handle(TerminalInputEvent::CredentialActions, Some(&mut config))
            .expect("open credential actions");
        session
            .handle(TerminalInputEvent::Down, Some(&mut config))
            .expect("select Replace");
        session
            .handle(TerminalInputEvent::Down, Some(&mut config))
            .expect("select Test");

        let mut external = config
            .begin_draft(ConfigScope::User)
            .expect("external Config draft");
        external
            .set_raw("providers.edge.base_url", "https://changed.example.com/v1")
            .expect("stage changed Provider origin");
        config
            .commit(external, false)
            .expect("commit changed Provider origin");

        assert_eq!(
            session
                .handle(TerminalInputEvent::Enter, Some(&mut config))
                .expect("stale credential test is recoverable"),
            TerminalLoopOutcome::Redraw
        );
        assert_eq!(
            session.notice.as_deref(),
            Some("Credential scope changed; reopen credential actions")
        );
        assert!(session.credential_flow.is_none());
        assert!(session.take_credential_command().is_none());
        assert!(!paths.project().exists());
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_revalidates_credential_scope_at_vault_dispatch() {
        let root = terminal_test_root("provider-credential-dispatch-scope");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let mut session = TerminalSession::new("/config provider credential", 80, 24)
            .expect("terminal session");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Provider selector");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open credential field");
        session
            .handle(TerminalInputEvent::CredentialActions, Some(&mut config))
            .expect("open credential actions");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Bind input");
        let secret = "private-dispatch-stale-token";
        for character in secret.chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("enter credential");
        }
        assert_eq!(
            session
                .handle(TerminalInputEvent::Enter, Some(&mut config))
                .expect("stage credential bind"),
            TerminalLoopOutcome::ResolveCredential
        );

        let mut external = config
            .begin_draft(ConfigScope::User)
            .expect("external Config draft");
        external
            .set_raw("providers.edge.base_url", "https://changed.example.com/v1")
            .expect("stage changed Provider origin");
        config
            .commit(external, false)
            .expect("commit changed Provider origin");

        let mut vault = CountingCredentialVault::default();
        super::resolve_pending_credential_command(&mut session, &config, &mut vault)
            .expect("reject stale credential command");
        assert_eq!(vault.calls.get(), 0);
        assert_eq!(
            session.notice.as_deref(),
            Some("Credential scope changed; reopen credential actions")
        );
        assert!(session.credential_flow.is_none());
        assert!(!paths.project().exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[test]
    fn terminal_discards_secret_when_the_provider_scope_becomes_stale() {
        let root = terminal_test_root("provider-credential-stale-scope");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let mut config = ConfigRuntime::open(paths.clone(), ConfigDocument::empty())
            .expect("config runtime");
        let mut session =
            TerminalSession::new("/config provider credential", 80, 24).expect("terminal session");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open Provider selector");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open credential field");
        session
            .handle(TerminalInputEvent::CredentialActions, Some(&mut config))
            .expect("open credential actions");
        session
            .handle(TerminalInputEvent::Enter, Some(&mut config))
            .expect("open bind input");
        for character in "private-stale-scope-token".chars() {
            session
                .handle(TerminalInputEvent::Character(character), Some(&mut config))
                .expect("enter credential");
        }

        let mut external = config
            .begin_draft(ConfigScope::User)
            .expect("external Config draft");
        external
            .set_raw("providers.edge.base_url", "https://changed.example.com/v1")
            .expect("stage changed Provider origin");
        config
            .commit(external, false)
            .expect("commit changed Provider origin");

        assert_eq!(
            session
                .handle(TerminalInputEvent::Enter, Some(&mut config))
                .expect("stale credential submission is recoverable"),
            TerminalLoopOutcome::Redraw
        );
        assert_eq!(
            session.notice.as_deref(),
            Some("Credential scope changed; reopen credential actions")
        );
        assert!(session.credential_flow.is_none());
        assert!(session.take_credential_command().is_none());
        assert!(!paths.project().exists());
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[cfg(not(windows))]
    #[test]
    fn terminal_credential_binding_fails_closed_when_platform_vault_is_unavailable() {
        let root = terminal_test_root("provider-credential-platform-unavailable");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let before = std::fs::read(paths.user()).expect("read provider config before bind");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let secret = "private-unavailable-platform-token";
        let mut events: VecDeque<_> = "config provider credential"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        events.extend(secret.chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            80,
            24,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("Platform credential vault is unavailable"));
        assert!(!output.contains(secret));
        assert!(!output.contains("synthetic-edge-credential-reference"));
        assert_eq!(
            std::fs::read(paths.user()).expect("read provider config after bind"),
            before
        );
        assert!(!paths.project().exists());
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[cfg(windows)]
    #[test]
    fn terminal_loop_binds_provider_credential_through_windows_vault() {
        let root = terminal_test_root("provider-credential-windows-vault");
        let ledger = root.join("runtime.ledger");
        let paths = ConfigPaths::new(root.join("user.toml"), root.join("project.toml"));
        write_terminal_provider_config(&paths);
        let reference = format!(
            "terminal-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        );
        let provider_config = std::fs::read_to_string(paths.user())
            .expect("read Windows Provider config")
            .replace("synthetic-edge-credential-reference", &reference);
        std::fs::write(paths.user(), provider_config).expect("write Windows Provider config");
        let before = std::fs::read(paths.user()).expect("read Provider config before bind");
        let mut config =
            ConfigRuntime::open(paths.clone(), ConfigDocument::empty()).expect("config runtime");
        let profile = config
            .provider_profile("edge")
            .expect("resolve Provider Profile")
            .expect("external Provider Profile");
        let scope = ProviderCredentialScope::from_profile(&profile).expect("credential scope");
        let mut cleanup_vault = PlatformCredentialVault;
        let _ = cleanup_vault.forget(&scope);
        let _cleanup = WindowsTerminalCredentialCleanup(scope.clone());
        let view = build_terminal_view(&ledger, &config, "/").expect("terminal view");
        let secret = format!("synthetic-windows-terminal-{}", std::process::id());
        let mut events: VecDeque<_> = "config provider credential"
            .chars()
            .map(|character| {
                Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
            })
            .collect();
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        ]);
        events.extend(secret.chars().map(|character| {
            Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
        }));
        events.extend([
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL)),
        ]);

        let output = run_terminal_loop(
            Vec::new(),
            FakeTerminalMode::default(),
            &mut config,
            &view,
            80,
            24,
            move || Ok(events.pop_front().expect("bounded event sequence")),
        )
        .expect("terminal loop");

        let stored = PlatformCredentialVault
            .resolve(&scope)
            .expect("bound Windows credential");
        assert_eq!(stored.expose(), secret.as_bytes());
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("Credential bound"));
        assert!(!output.contains(&secret));
        assert!(!output.contains(&reference));
        assert_eq!(
            std::fs::read(paths.user()).expect("read Provider config after bind"),
            before
        );
        assert!(!paths.project().exists());
        assert!(!ledger.exists());
        std::fs::remove_dir_all(root).expect("remove test config");
    }

    #[cfg(windows)]
    struct WindowsTerminalCredentialCleanup(ProviderCredentialScope);

    #[cfg(windows)]
    impl Drop for WindowsTerminalCredentialCleanup {
        fn drop(&mut self) {
            let _ = PlatformCredentialVault.forget(&self.0);
        }
    }
