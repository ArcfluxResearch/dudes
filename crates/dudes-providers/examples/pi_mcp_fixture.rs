//! Launch/install and streamed-end fixtures from the public Pi adapter.

use std::collections::BTreeMap;
use std::error::Error;
use std::ffi::OsString;
use std::path::Path;

use dudes_providers::adapter::{Adapter, AgentCtx, TurnSession};
use dudes_providers::launch::{EnvVar, LaunchInput};
use dudes_providers::mcp::{LoadedServers, McpSupport, Rules, parse_servers, split_secrets};
use dudes_providers::pi::PiAdapter;
use dudes_providers::pi::config::PiConfig;
use dudes_types::{AgentId, McpServerName, ModelSelection, SessionId};
use serde_json::{Value, json};

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.len() != 3 {
        return Err("expected input and output fixture paths".into());
    }
    let input: Value = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    if input["read_stdout"].is_string() {
        std::fs::write(&args[2], serde_json::to_vec(&read_stream(&input)?)?)?;
        return Ok(());
    }
    let values: Vec<(String, String)> = serde_json::from_value(input["env"].clone())?;
    let env: Vec<EnvVar> = values
        .iter()
        .map(|(name, value)| EnvVar::new(name, value))
        .collect();
    let declared = parse_servers(serde_json::from_value(input["declarations"].clone())?)?;
    let (remainder, secrets) = split_secrets(&env, &declared);
    let names = input["servers"]
        .as_array()
        .ok_or("server names are required")?
        .iter()
        .map(|name| McpServerName::parse(name.as_str().unwrap_or_default()))
        .collect::<Result<Vec<_>, _>>()?;
    let servers = LoadedServers::new(&declared, &secrets, |_| None).admit(
        &names,
        Rules {
            support: McpSupport::Http,
            default_isolation: true,
            trusted: false,
        },
    )?;
    let field = |name| input[name].as_str().ok_or("a fixture string is missing");
    let adapter = PiAdapter::new(PiConfig {
        command: field("program")?.to_owned(),
    });
    let context = AgentCtx {
        extra_dirs: &[],
        agent: AgentId::parse("01ARZ3NDEKTSV4RRFFQ69G5FAV")?,
        cwd: Path::new(field("cwd")?),
        agent_dir: Path::new(field("agent_dir")?),
        daemon_env: &remainder,
        sandbox: None,
        trusted: false,
        max_turns: None,
        max_budget: None,
        mcp_servers: &servers,
    };
    let session = input["session"]
        .as_str()
        .map_or(TurnSession::Assigned, |id| {
            TurnSession::Resume(SessionId::new(id))
        });
    let model = input["model"]
        .as_str()
        .map_or_else(ModelSelection::provider_default, ModelSelection::named);
    let spec = adapter.managed(&context, field("task")?, &session, &model)?;
    if !matches!(spec.stdin, LaunchInput::Null) {
        return Err("the fixture requires null stdin".into());
    }
    let environment = spec.env.iter().map(|var| {
        let source = values.iter().position(|(_, value)| var.expose_value() == value.as_str());
        if let Some(source) = source {
            Ok(json!({"name":var.name().to_str().ok_or("non-UTF-8 environment name")?, "source":source}))
        } else if var.name() == "JITI_FS_CACHE" && var.expose_value() == "false" {
            Ok(json!({"name":"JITI_FS_CACHE", "literal":"false"}))
        } else { Err("an environment value has no synthetic fixture source") }
    }).collect::<Result<Vec<_>, _>>()?;
    let install = adapter.managed_install(&context, &model);
    let files: BTreeMap<_, _> = install.files.into_iter().collect();
    let output = json!({
        "argv":spec.argv.iter().map(|arg| arg.to_str().ok_or("non-UTF-8 argv")).collect::<Result<Vec<_>, _>>()?,
        "env":environment, "cwd":spec.cwd, "stdin":"null",
        "directories":install.directories, "files":files,
    });
    std::fs::write(&args[2], serde_json::to_vec(&output)?)?;
    Ok(())
}

fn read_stream(input: &Value) -> Result<Value, Box<dyn Error>> {
    use dudes_providers::stream::TurnReader;
    let mut reader: Box<dyn TurnReader> = Box::new(dudes_providers::pi::stream::TurnStream::new());
    let text = std::fs::read_to_string(
        input["read_stdout"]
            .as_str()
            .ok_or("stdout path is missing")?,
    )?;
    let mut incremental = 0;
    for line in text.lines() {
        if reader
            .line(line)
            .session
            .is_some_and(|observation| !observation.servers.is_empty())
        {
            incremental += 1;
        }
    }
    let code = i32::try_from(input["exit_code"].as_i64().ok_or("exit code is missing")?)?;
    let end = reader.end(dudes_proto::ExitStatus {
        code: Some(code),
        signal: None,
    });
    let outcome = match end.outcome {
        dudes_proto::TurnOutcome::Completed => "completed",
        dudes_proto::TurnOutcome::NoResult => "no_result",
        _ => "failed",
    };
    Ok(json!({"outcome":outcome, "unreadable":end.unreadable,
        "servers":end.final_observation.map(|observation| observation.servers).unwrap_or_default(),
        "incremental_server_observations":incremental}))
}
