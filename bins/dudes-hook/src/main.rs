//! One synchronous vendor hook call over the platform local transport.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;

use dudes_proto::reply::VendorReply;
use dudes_proto::rpc::hook::{HookEventParams, HookToken, OVERLOADED};
use dudes_proto::rpc::{ErrorCode, Method, Response};
use dudes_rpc::{ClientError, FrameError, LocalClient, MAX_FRAME_BYTES};
use serde_json::Value;

const FAILURE: &str = "dudes-hook: hook delivery failed\n";
const OVERLOAD_BLOCK: &str = "dudes-hook: daemon overloaded; control step blocked; retry, and restart duded if this persists\n";
const UNANSWERED_BLOCK: &str = "dudes-hook: daemon connection ended before an answer; control step blocked; retry, and restart duded if this persists\n";
const FAIL_OPEN: i32 = 0;
const WINDOWS_PIPE_NOT_CONNECTED: i32 = 233;

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();

    let code = run(stdin.lock(), &mut out, &mut err).unwrap_or_else(|()| {
        let _ = err.write_all(FAILURE.as_bytes());
        FAIL_OPEN
    });

    let _ = out.flush();
    let _ = err.flush();
    std::process::exit(code);
}

fn run(input: impl Read, out: &mut impl Write, err: &mut impl Write) -> Result<i32, ()> {
    let (endpoint, token) = environment()?;
    let raw = payload(input)?;
    let control = matches!(
        raw.get("hook_event_name").and_then(Value::as_str),
        Some("PreToolUse" | "UserPromptSubmit")
    );
    let params = serde_json::to_value(HookEventParams { token, raw }).map_err(|_| ())?;

    let response = match LocalClient::connect(endpoint) {
        Ok(mut client) => client.call(Method::HookEvent, Some(params)),
        Err(error) if windows_pipe_disconnected(&error) => Err(ClientError::Io(error)),
        Err(_) => return Err(()),
    };
    let response = match response {
        Ok(response) => response,
        // A full daemon cap can close a pipe before client setup or a write finishes.
        Err(error) if control && ended_unanswered(&error) => {
            let _ = err.write_all(UNANSWERED_BLOCK.as_bytes());
            return Ok(2);
        }
        Err(_) => return Err(()),
    };
    if control
        && response.error.as_ref().is_some_and(|error| {
            error.code == ErrorCode::InternalError.code() && error.message == OVERLOADED
        })
    {
        let _ = err.write_all(OVERLOAD_BLOCK.as_bytes());
        return Ok(2);
    }
    let reply = reply(response)?;
    if let Some(body) = reply.body {
        serde_json::to_writer(&mut *out, &body).map_err(|_| ())?;
    }
    out.flush().map_err(|_| ())?;
    Ok(reply.exit_code)
}

fn ended_unanswered(error: &ClientError) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionReset, NotConnected, UnexpectedEof};
    match error {
        ClientError::Closed => true,
        ClientError::Frame(FrameError::Io(error)) | ClientError::Io(error) => {
            matches!(
                error.kind(),
                BrokenPipe | NotConnected | ConnectionReset | UnexpectedEof
            ) || windows_pipe_disconnected(error)
        }
        _ => false,
    }
}

fn windows_pipe_disconnected(error: &std::io::Error) -> bool {
    cfg!(windows) && error.raw_os_error() == Some(WINDOWS_PIPE_NOT_CONNECTED)
}

fn environment() -> Result<(PathBuf, HookToken), ()> {
    let endpoint = required("DUDES_SOCK")?;
    let token = required("DUDES_TOKEN")?.into_string().map_err(|_| ())?;
    let token = HookToken::new(token).map_err(|_| ())?;
    Ok((PathBuf::from(endpoint), token))
}

fn required(name: &'static str) -> Result<OsString, ()> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .ok_or(())
}

fn payload(mut input: impl Read) -> Result<Value, ()> {
    let mut bytes = Vec::new();
    input
        .by_ref()
        .take(MAX_FRAME_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(());
    }
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn reply(response: Response) -> Result<VendorReply, ()> {
    if response.error.is_some() {
        return Err(());
    }
    serde_json::from_value(response.result.ok_or(())?).map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use std::io::{Error, ErrorKind};

    use super::*;

    #[test]
    fn only_a_connection_that_ended_without_an_answer_is_unanswered() {
        assert!(ended_unanswered(&ClientError::Closed));
        for kind in [
            ErrorKind::BrokenPipe,
            ErrorKind::NotConnected,
            ErrorKind::ConnectionReset,
            ErrorKind::UnexpectedEof,
        ] {
            assert!(
                ended_unanswered(&ClientError::Io(Error::from(kind))),
                "{kind}"
            );
            assert!(
                ended_unanswered(&ClientError::Frame(FrameError::Io(Error::from(kind)))),
                "{kind}"
            );
        }
        assert!(!ended_unanswered(&ClientError::Io(Error::from(
            ErrorKind::PermissionDenied
        ))));
        assert!(!ended_unanswered(&ClientError::Frame(FrameError::Io(
            Error::from(ErrorKind::PermissionDenied)
        ))));
        assert!(!ended_unanswered(&ClientError::Frame(FrameError::TooLarge)));
        assert!(!ended_unanswered(&ClientError::Frame(FrameError::NotUtf8)));
        let malformed = serde_json::from_str::<Response>("{").unwrap_err();
        assert!(!ended_unanswered(&ClientError::Malformed(malformed)));
    }

    #[test]
    fn a_native_windows_pipe_close_is_unanswered_without_classifying_other_errors() {
        for (code, expected) in [(233, cfg!(windows)), (2, false), (5, false), (234, false)] {
            assert_eq!(
                windows_pipe_disconnected(&Error::from_raw_os_error(code)),
                expected,
                "connection setup error {code}"
            );
            assert_eq!(
                ended_unanswered(&ClientError::Io(Error::from_raw_os_error(code))),
                expected,
                "native error {code}"
            );
            assert_eq!(
                ended_unanswered(&ClientError::Frame(FrameError::Io(
                    Error::from_raw_os_error(code)
                ))),
                expected,
                "framed native error {code}"
            );
        }
    }
}
