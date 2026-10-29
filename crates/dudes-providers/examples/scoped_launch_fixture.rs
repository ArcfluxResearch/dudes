//! Export an unarmed candidate with environment values referenced through synthetic input.

use std::error::Error;
use std::path::Path;

use dudes_proto::repository::{RepositoryEntry, RepositoryTurnGrant, ScopedRepositories};
use dudes_providers::adapter::{Adapter, AgentCtx, TurnSession};
use dudes_providers::claude::{ClaudeAdapter, config::ClaudeConfig};
use dudes_providers::codex::{CodexAdapter, config::CodexConfig};
use dudes_providers::launch::{EnvVar, LaunchInput};
use dudes_providers::protocol::ProtocolAction;
use dudes_providers::scoped::{ScopedLaunchInputs, ScopedPlatform, ScopedPrivatePaths};
use dudes_types::{AgentId, Mode, ModelSelection, SessionId};
use serde_json::{Value, json};

fn protocol_actions(actions: &[ProtocolAction]) -> Result<Vec<Value>, Box<dyn Error>> {
    actions.iter().map(|action| Ok(match action {
        ProtocolAction::Write(frame) => json!({"kind":"write","task":frame.is_task(),"message":serde_json::from_slice::<Value>(frame.bytes())?}),
        ProtocolAction::Started => json!({"kind":"started"}),
        ProtocolAction::Close => json!({"kind":"close"}),
        ProtocolAction::Fail => json!({"kind":"fail"}),
    })).collect()
}

fn protocol_projection(
    adapter: &dyn Adapter,
    spec: &dudes_providers::launch::LaunchSpec,
    input: &Value,
) -> Result<(Vec<Value>, Vec<Value>), Box<dyn Error>> {
    let mut reader = adapter.turn_reader_for(spec);
    let initial = reader.start();
    let start = initial
        .iter()
        .filter_map(|action| match action {
            ProtocolAction::Write(frame) => Some(serde_json::from_slice::<Value>(frame.bytes())),
            _ => None,
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut actions = protocol_actions(&initial)?;
    if let Some(responses) = input["vendor_responses"].as_array() {
        for response in responses {
            actions.extend(protocol_actions(
                &reader.line(&response.to_string()).actions,
            )?);
        }
    }
    Ok((start, actions))
}

fn adapter(input: &Value, profile: &Path) -> Result<Box<dyn Adapter>, Box<dyn Error>> {
    match input["provider"].as_str() {
        Some("claude") => {
            let mut raw = input["config"].clone();
            let command = raw
                .as_object_mut()
                .ok_or("fixture config must be an object")?
                .remove("command");
            let mut config: ClaudeConfig = serde_json::from_value(raw)?;
            if let Some(command) = command {
                config.command = command
                    .as_str()
                    .ok_or("fixture program must be text")?
                    .into();
            }
            Ok(Box::new(ClaudeAdapter::new(config)))
        }
        Some("codex") => Ok(Box::new(
            CodexAdapter::new(serde_json::from_value::<CodexConfig>(
                input["config"].clone(),
            )?)
            .with_managed_home(profile.to_owned()),
        )),
        _ => Err("fixture provider has no candidate".into()),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.len() != 3 {
        return Err("expected synthetic input and output paths".into());
    }
    let input: Value = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let field = |name| input[name].as_str().ok_or("missing fixture field");
    let private = |name| {
        input["private"][name]
            .as_str()
            .map(Into::into)
            .ok_or("missing private fixture path")
    };
    let values: Vec<(String, String)> = serde_json::from_value(input["env"].clone())?;
    let env: Vec<_> = values
        .iter()
        .map(|(name, value)| EnvVar::new(name, value))
        .collect();
    let grant: RepositoryTurnGrant = serde_json::from_value(input["grant"].clone())?;
    let hired: ScopedRepositories = serde_json::from_value(input["hired"].clone())?;
    let git: Vec<RepositoryEntry<std::path::PathBuf>> =
        serde_json::from_value(input["origin_git_dirs"].clone())?;
    let scope = ScopedLaunchInputs {
        grant: &grant,
        hired: &hired,
        origin_git_dirs: &git,
        private: ScopedPrivatePaths {
            home: private("home")?,
            profile: private("profile")?,
            work: private("work")?,
            outbox: private("outbox")?,
            temp: private("temp")?,
        },
        codex_catalog: input["codex_catalog"].as_str().map(Into::into),
        mode: serde_json::from_value::<Mode>(input["mode"].clone())?,
        platform: match field("platform")? {
            "macos_arm64" => ScopedPlatform::MacOsArm64,
            "linux_x86_64" => ScopedPlatform::LinuxX86_64,
            _ => ScopedPlatform::Unsupported,
        },
    };
    let adapter = adapter(&input, &scope.private.profile)?;
    let ctx = AgentCtx {
        agent: AgentId::from_parts(1, 1),
        cwd: Path::new(field("legacy_cwd")?),
        agent_dir: Path::new(field("agent_dir")?),
        daemon_env: &env,
        sandbox: Some(true),
        trusted: true,
        max_turns: Some(8),
        max_budget: None,
        mcp_servers: &[],
    };
    let session = match field("session_kind")? {
        "first" => TurnSession::First(SessionId::new(field("session_id")?)),
        "assigned" => TurnSession::Assigned,
        "resume" => TurnSession::Resume(SessionId::new(field("session_id")?)),
        _ => return Err("unknown fixture session".into()),
    };
    let selection = ModelSelection::named(field("model")?);
    let candidate = adapter.managed_scoped(&ctx, &scope, field("task")?, &session, &selection)?;
    let environment:Vec<_>=candidate.spec.env.iter().map(|var| {
        let source=values.iter().position(|(_,value)|var.expose_value()==value.as_str()).ok_or("a generated environment value has no synthetic input reference")?;
        Ok(json!({"name":var.name().to_str().ok_or("non-text fixture environment name")?,"source":source}))
    }).collect::<Result<_,Box<dyn Error>>>()?;
    let stdin = match &candidate.spec.stdin {
        LaunchInput::WriteThenClose(bytes) if bytes == field("task")?.as_bytes() => {
            json!({"kind":"write_then_close","source":"task"})
        }
        LaunchInput::Protocol(startup) => {
            json!({"kind":"protocol","home":startup.home(),"cwd":startup.cwd(),"source":"task","expected_additional_roots":candidate.inventory.additional_roots()})
        }
        _ => return Err("candidate has an unexpected task delivery".into()),
    };
    let (protocol_start, protocol_actions) =
        protocol_projection(adapter.as_ref(), &candidate.spec, &input)?;
    let output = json!({"argv":candidate.spec.argv.iter().map(|arg|arg.to_str().ok_or("non-text fixture argument")).collect::<Result<Vec<_>,_>>()?,"env":environment,"cwd":candidate.spec.cwd,"stdin":stdin,"protocol_start":protocol_start,"protocol_actions":protocol_actions,"directories":candidate.install.directories,"files":candidate.install.files,"inventory":candidate.inventory,"unarmed":candidate.unarmed});
    std::fs::write(&args[2], serde_json::to_vec_pretty(&output)?)?;
    Ok(())
}
