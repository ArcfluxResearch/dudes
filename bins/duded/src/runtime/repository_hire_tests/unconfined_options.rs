use dudes_proto::repository::Confinement;
use dudes_providers::claude::config::{ClaudeConfig, PermissionMode};
use dudes_types::McpServerName;

use super::*;
use crate::runtime::repository_admission::CurrentHireAdmission;
use crate::runtime::repository_hire::require_group_options;

const CLAUDE_ONLY: &str = "Multi-repository projects work with Claude only for now.";

type TestDaemon = Daemon<common::SharedHost, Runner, common::TestClock, common::FixtureRng>;

fn trusted_setup() -> (Setup, HireParams) {
    let setup = Setup::new();
    let (mut running, _, params) = setup.start_from(setup.boot_unconfined());
    running.stop(ExitCause::Ended);
    (setup, params)
}

fn with_server() -> crate::settings::RunOptions {
    let servers = dudes_providers::mcp::parse_servers(
        toml::from_str("[a]\nurl='http://127.0.0.1:4501/mcp'\n").unwrap(),
    )
    .unwrap();
    crate::settings::RunOptions::default().with_mcp_servers(servers)
}

fn admitted_options(daemon: &TestDaemon, params: &HireParams) -> anyhow::Result<()> {
    let admission =
        CurrentHireAdmission::new(&daemon.ctx.enrollment, &daemon.ctx.current_repositories);
    require_group_options(&admission, params)?;
    daemon
        .hire_option_guards(params)
        .map_err(|error| anyhow::anyhow!(error.message))
}

#[test]
fn an_unconfined_hire_takes_the_options_of_a_single_repo_hire() {
    let (setup, mut params) = trusted_setup();
    let claude = ClaudeConfig {
        permission_mode: PermissionMode::Default,
        ..ClaudeConfig::default()
    };
    let daemon = setup.boot_unconfined_with(claude, with_server());
    params.model = Some(dudes_types::ModelName::parse("claude-opus-4-1").unwrap());
    params.mcp_servers = vec![McpServerName::parse("a").unwrap()];
    admitted_options(&daemon, &params).unwrap();

    let refused = daemon.repository_hire_plan(params.clone()).unwrap_err();
    assert!(
        refused.message.contains("cannot take MCP servers"),
        "{refused:?}"
    );
    params.mcp_servers.clear();
    daemon.repository_hire_plan(params.clone()).unwrap();
    params.isolation = Some(IsolationMode::Bare);
    let refused = daemon.repository_hire_plan(params).unwrap_err();
    assert!(refused.message.contains("isolation setting"), "{refused:?}");
}

#[test]
fn codex_and_pi_are_refused_before_any_lease_with_one_sentence() {
    let setup = Setup::new();
    let (mut running, hires, params) = setup.start_from(setup.boot_unconfined());
    for provider in [Provider::Codex, Provider::Pi] {
        let mut request = params.clone();
        request.provider = provider;
        let error = hires.hire(request).unwrap_err();
        assert!(error.message.contains(CLAUDE_ONLY), "{error:?}");
    }
    running.stop(ExitCause::Ended);
    setup.assert_cleaned(0);
}

#[test]
fn a_configured_model_refuses_the_confined_offer_and_not_the_unconfined_one() {
    let (setup, _) = trusted_setup();
    let claude = || ClaudeConfig {
        extra_args: vec!["--model".into(), "claude-opus-4-1".into()],
        ..ClaudeConfig::default()
    };
    let profile = &setup.settings.enrollment.profiles[0];
    let unconfined = setup
        .boot_unconfined_with(claude(), crate::settings::RunOptions::default())
        .browser_repository_offer(profile)
        .unwrap();
    assert!(unconfined.providers[0].enabled);
    assert_eq!(unconfined.providers[0].model, None);
    let confined = setup
        .boot_with(true, claude())
        .browser_repository_offer(profile)
        .unwrap();
    assert!(!confined.providers[0].enabled);
}

#[test]
fn the_floor_offer_and_the_hire_path_agree_for_every_engine() {
    let (setup, params) = trusted_setup();
    let profile = &setup.settings.enrollment.profiles[0];
    for confinement in [Confinement::Confined, Confinement::Unconfined] {
        let daemon = match confinement {
            Confinement::Confined => setup.boot(true),
            Confinement::Unconfined => setup.boot_unconfined(),
        };
        let offer = daemon.browser_repository_offer(profile).unwrap();
        for provider in offer.providers {
            let mut request = params.clone();
            request.provider = provider.provider;
            let planned = daemon.repository_hire_plan(request);
            assert_eq!(provider.enabled, planned.is_ok(), "{provider:?}");
            if let Err(error) = planned {
                assert!(
                    error.message.ends_with(provider.refusal.unwrap()),
                    "{error:?} against {provider:?}"
                );
            }
        }
    }
}
