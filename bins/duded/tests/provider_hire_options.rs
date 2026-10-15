//! A successful Codex or Pi hire would record options neither adapter enforces on a turn.

use std::sync::Arc;

use duded::ports::Ask;
use duded::settings::Seams;
use duded::startup::Witness;
use duded::{ExitCause, boot};
use dudes_proto::Record;
use dudes_proto::rpc::ErrorCode;
use dudes_providers::adapter::Adapter;
use dudes_providers::codex::CodexAdapter;
use dudes_providers::pi::PiAdapter;
use dudes_types::{IsolationMode, Provider, Usd};

mod common;

use common::{Fixture, ScriptedRunner, TestClock};

#[test]
fn codex_and_pi_hires_refuse_ignored_options_without_journaling() {
    let fixture = Fixture::new();
    let settings = fixture.settings();
    let seams = Seams {
        adapters: [
            Box::new(CodexAdapter::default()) as Box<dyn Adapter>,
            Box::new(PiAdapter::default()) as Box<dyn Adapter>,
        ]
        .into_iter()
        .collect(),
        ..common::seams(
            &settings,
            common::SharedHost::new(),
            Arc::new(ScriptedRunner::scripted([])),
            Arc::new(TestClock::new()),
            Witness::default(),
        )
    };
    let mut running = common::start(boot(&fixture.settings(), seams).expect("boot"));

    for provider in [Provider::Codex, Provider::Pi] {
        for field in ["max_turns", "max_budget", "isolation", "sandbox"] {
            let mut params = common::hire_params(fixture.state.path());
            params.provider = provider;
            match field {
                "max_turns" => params.max_turns = Some(2),
                "max_budget" => params.max_budget = Some(Usd::from_dollars(1)),
                "isolation" => params.isolation = Some(IsolationMode::Inherit),
                "sandbox" => params.sandbox = Some(false),
                _ => unreachable!(),
            }
            let refused = running
                .handle
                .ask(Ask::Hire(Box::new(params)))
                .expect_err("an ignored hire option is refused");
            let code = if field == "sandbox" {
                ErrorCode::Refused
            } else {
                ErrorCode::InvalidParams
            };
            assert_eq!(refused.code, code.code(), "{provider} {field}");
            assert!(
                refused.message.contains(&provider.to_string()) && refused.message.contains(field),
                "{provider} {field}: {refused:?}"
            );
        }
    }

    assert!(
        fixture
            .records()
            .iter()
            .all(|record| !matches!(record, Record::AgentHired { .. })),
        "a refused hire must not reach the journal"
    );
    assert_eq!(running.stop(ExitCause::ShutdownRpc), ExitCause::ShutdownRpc);
}
