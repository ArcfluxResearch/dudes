//! `dudes upgrade` refusals that need no network: they happen before any program is started.
#![cfg(unix)]

use std::ffi::OsString;

use dudes::{Environment, exit};

fn run(variables: &[(&str, &str)], words: &[&str]) -> (u8, String, String) {
    let variables: Vec<(String, OsString)> = variables
        .iter()
        .map(|(name, value)| ((*name).to_owned(), OsString::from(*value)))
        .collect();
    let environment = Environment::resolve(move |name| {
        variables
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.clone())
    });
    let argv: Vec<OsString> = words.iter().map(OsString::from).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = dudes::run(&argv, &environment, None, &mut out, &mut err);
    (
        code,
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn an_agents_token_refuses_before_anything_runs() {
    let token = "t".repeat(dudes_proto::rpc::hook::HOOK_TOKEN_LEN);
    let (code, out, err) = run(&[("DUDES_TOKEN", &token)], &["upgrade", "--check"]);
    assert_eq!(code, 3);
    assert_eq!(out, "");
    assert_eq!(
        err,
        "dudes: upgrade is for the operator and refuses to run with an agent's DUDES_TOKEN set\n"
    );
}

#[test]
fn json_and_contradictory_flags_are_usage_errors() {
    let (code, _, err) = run(&[], &["--json", "upgrade"]);
    assert_eq!(code, exit::USAGE);
    assert!(
        err.starts_with("dudes: `upgrade` prints text only"),
        "{err}"
    );
    let (code, _, err) = run(&[], &["upgrade", "--check", "--now"]);
    assert_eq!(code, exit::USAGE);
    assert!(
        err.starts_with("dudes: `--check` cannot be combined with `--now`"),
        "{err}"
    );
    let (code, _, err) = run(&[], &["upgrade", "--version", "0.1.0"]);
    assert_eq!(code, exit::USAGE);
    assert!(
        err.starts_with("dudes: unknown option `--version`"),
        "{err}"
    );
}
