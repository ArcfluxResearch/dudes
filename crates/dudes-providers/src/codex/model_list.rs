//! Asking Codex's app server which models it offers, on a process of its own and with no thread.
//!
//! The request is `model/list` after `initialize` and `initialized`. Codex answers it from its own
//! catalog without a paid model call. [`parse`] reads the answer, and the daemon runs the process.

use std::ffi::OsString;

use super::startup::CodexStartup;

/// The app server's arguments after the program, the same posture a streamed turn runs under.
pub(crate) const APP_SERVER_ARGS: &[&str] = &[
    "app-server",
    "--listen",
    "stdio://",
    "--strict-config",
    "-c",
    "features.hooks=false",
    "-c",
    "features.plugins=false",
    "-c",
    "features.apps=false",
    "-c",
    "features.shell_snapshot=false",
    "-c",
    "approval_policy=\"never\"",
    "-c",
    "sandbox_mode=\"workspace-write\"",
    "-c",
    "sandbox_workspace_write.exclude_tmpdir_env_var=true",
    "-c",
    "sandbox_workspace_write.exclude_slash_tmp=true",
    "-c",
    "sandbox_workspace_write.network_access=false",
];

/// The id the `model/list` request carries, and its answer.
const REQUEST_ID: u64 = 2;

/// The arguments of the listing process, program first.
#[must_use]
pub fn argv(command: impl Into<OsString>) -> Vec<OsString> {
    std::iter::once(command.into())
        .chain(APP_SERVER_ARGS.iter().map(OsString::from))
        .collect()
}

/// The three lines the listing process reads. Its input must stay open until [`answered`] holds,
/// because the app server exits when its input ends, before it answers the third line.
#[must_use]
pub fn input() -> Vec<u8> {
    let mut bytes = CodexStartup::initialize().bytes().to_vec();
    bytes.extend_from_slice(b"{\"method\":\"initialized\"}\n");
    bytes.extend_from_slice(
        format!("{{\"id\":{REQUEST_ID},\"method\":\"model/list\",\"params\":{{}}}}\n").as_bytes(),
    );
    bytes
}

/// Whether the output so far holds a whole line answering the request, success or error.
#[must_use]
pub fn answered(stdout: &[u8]) -> bool {
    let Some(end) = stdout.iter().rposition(|byte| *byte == b'\n') else {
        return false;
    };
    String::from_utf8_lossy(&stdout[..end]).lines().any(|line| {
        serde_json::from_str::<serde_json::Value>(line).is_ok_and(|value| {
            value.get("id").and_then(serde_json::Value::as_u64) == Some(REQUEST_ID)
        })
    })
}

/// The model ids the answer lists, in its order, without the ones it marks hidden. Empty when the
/// output holds no answer to the request.
#[must_use]
pub fn parse(stdout: &[u8]) -> Vec<String> {
    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if value.get("id").and_then(serde_json::Value::as_u64) != Some(REQUEST_ID) {
            continue;
        }
        let Some(data) = value
            .pointer("/result/data")
            .and_then(|data| data.as_array())
        else {
            return Vec::new();
        };
        return data
            .iter()
            .filter(|entry| entry.get("hidden").and_then(serde_json::Value::as_bool) != Some(true))
            .filter_map(|entry| {
                entry
                    .get("model")
                    .or_else(|| entry.get("id"))
                    .and_then(serde_json::Value::as_str)
            })
            .map(str::to_owned)
            .collect();
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_captured_answer_lists_its_seven_models_in_order() {
        let capture = include_str!("../../tests/fixtures/codex/app-server-model-list.jsonl");
        assert_eq!(
            parse(capture.as_bytes()),
            [
                "gpt-6.1-sol",
                "gpt-6-astra",
                "gpt-6-sol",
                "gpt-6-luna",
                "gpt-5.6-sol",
                "gpt-5.6-terra",
                "gpt-5.6-luna"
            ]
        );
    }

    #[test]
    fn hidden_entries_and_unrelated_lines_are_skipped() {
        let output = br#"not json
{"id":1,"result":{"data":[{"model":"wrong"}]}}
{"id":2,"result":{"data":[{"model":"a","hidden":false},{"id":"b","model":"b2","hidden":true},{"id":"c"},{"hidden":false},{"model":7}],"nextCursor":null}}
{"id":2,"result":{"data":[{"model":"later"}]}}
"#;
        assert_eq!(parse(output), ["a", "c"]);
        assert!(parse(b"{\"id\":2,\"error\":{\"message\":\"no\"}}\n").is_empty());
        assert!(parse(b"").is_empty());
    }

    #[test]
    fn only_a_whole_line_with_the_request_id_is_an_answer() {
        assert!(!answered(b""));
        assert!(!answered(b"{\"id\":1,\"result\":{}}\n"));
        assert!(!answered(b"{\"id\":2,\"result\":{\"data\":[]}}"));
        assert!(answered(
            b"{\"id\":1}\n{\"id\":2,\"result\":{\"data\":[]}}\n"
        ));
        assert!(answered(b"{\"id\":2,\"error\":{\"message\":\"no\"}}\n"));
    }

    #[test]
    fn the_input_is_the_three_lines_the_capture_sent() {
        let text = String::from_utf8(input()).expect("text");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with(r#"{"id":1,"method":"initialize""#));
        assert_eq!(lines[1], r#"{"method":"initialized"}"#);
        assert_eq!(lines[2], r#"{"id":2,"method":"model/list","params":{}}"#);
        assert_eq!(
            argv("codex")[..4],
            ["codex", "app-server", "--listen", "stdio://"]
        );
    }
}
