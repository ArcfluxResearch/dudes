//! A non-networked owned child for the condense capture integration checks.

use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use dudes_proto::{AgentBudget, Entry, PeerProvenance, Record};
use dudes_types::{AgentId, Lsn, Provider, Timestamp};

fn main() -> anyhow::Result<()> {
    let proof = PathBuf::from(
        std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("scratch HOME required"))?,
    )
    .join("condense-proof");
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--proof-descendant"] {
        std::fs::write(proof.join("descendant.pid"), std::process::id().to_string())?;
        loop {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let info: serde_json::Value =
        serde_json::from_slice(&std::fs::read(proof.join("input.json"))?)?;
    if args == ["--seed"] {
        return seed(&info);
    }
    if args == ["--snapshot"] {
        return snapshot(&info);
    }
    let model = info["model"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("model required"))?;
    anyhow::ensure!(
        args == [
            "-p",
            "--output-format",
            "json",
            "--model",
            model,
            "--setting-sources",
            "",
            "--strict-mcp-config",
            "--settings",
            "{}",
            "--tools",
            ""
        ],
        "exact isolated argv required"
    );
    let agent: AgentId = serde_json::from_value(info["agent"].clone())?;
    let journal = PathBuf::from(
        info["journal"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("scratch journal required"))?,
    );
    let admitted = dudes_journal::Reader::new(&journal)
        .iter_from(Lsn::ZERO)?
        .any(|item| {
            matches!(item, Ok((_, dudes_proto::Entry { record: Record::MemoryCondenseModelAdmitted {
            agent: owner, allowed: true, .. }, .. })) if owner == agent)
        });
    anyhow::ensure!(
        admitted,
        "the child must see durable admission before reading its task"
    );
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input)?;
    anyhow::ensure!(!input.is_empty(), "one stdin task required");
    anyhow::ensure!(
        std::fs::read_dir(std::env::current_dir()?)?
            .next()
            .is_none(),
        "isolated cwd must remain empty"
    );
    let observed = serde_json::to_vec(&serde_json::json!({
        "pid":std::process::id(),"argv":args,"cwd_empty":true,"admission_visible":true,
        "stdin_bytes":input.len(),"stdin_hash":dudes_supervisor::blake3_hex(&input)
    }))?;
    std::fs::write(proof.join("observed.json"), &observed)?;
    let mut launches = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(proof.join("launches.jsonl"))?;
    launches.write_all(&observed)?;
    launches.write_all(b"\n")?;
    let mode = info["mode"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("mode required"))?;
    if matches!(mode, "wait" | "timeout") {
        let mut child = descendant()?;
        loop {
            if child.try_wait()?.is_some() {
                anyhow::bail!("descendant ended before sweep");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    emit_output(mode)
}

fn emit_output(mode: &str) -> anyhow::Result<()> {
    let mut output = serde_json::to_vec(
        &serde_json::json!({"type":"result","subtype":"success","is_error":false,
        "result":"{\"condensed\":\"an isolated owned-child summary\"}"}),
    )?;
    match mode {
        "success" => {}
        "exact-cap" => output.resize(65_536, b' '),
        "over-cap" => output.resize(65_537, b' '),
        "invalid-utf8" => output[0] = 0xff,
        "malformed" => output = b"{broken".to_vec(),
        "failed-completion" => {
            output = serde_json::to_vec(
                &serde_json::json!({"type":"result", "subtype":"error_during_execution", "is_error":true, "result":"{\"condensed\":\"must not publish\"}"}),
            )?;
        }
        "nonzero" => {
            std::io::stdout().write_all(&output)?;
            std::process::exit(1);
        }
        "stderr-only" => {
            std::io::stderr().write_all(&output)?;
            return Ok(());
        }
        _ => anyhow::bail!("unknown proof mode"),
    }
    std::io::stderr().write_all(b"stderr must never enter a summary\n")?;
    std::io::stdout().write_all(&output)?;
    std::io::stdout().flush()?;
    Ok(())
}

fn seed(info: &serde_json::Value) -> anyhow::Result<()> {
    let agent: AgentId = serde_json::from_value(info["agent"].clone())?;
    let journal = PathBuf::from(info["journal"].as_str().unwrap());
    let cwd = PathBuf::from(info["cwd"].as_str().unwrap());
    let (mut writer, _) = dudes_journal::open(&journal, dudes_journal::JournalOptions::default())?;
    let mut records = vec![Record::AgentHired {
        id: agent,
        name: "capture fixture".to_owned(),
        character: None,
        accent: None,
        role: "scratch".to_owned(),
        department: "scratch".to_owned(),
        provider: Provider::Claude,
        cwd,
        workspace: None,
        budget: AgentBudget {
            usd_per_day: None,
            tokens_per_day: None,
        },
        sandbox: None,
        isolation: None,
        max_turns: None,
        max_budget: None,
        model: None,
        trusted: false,
        worktree: None,
        mcp_servers: Vec::new(),
        browser_onboarding: None,
    }];
    for index in 0..3 {
        records.push(Record::MemoryAppend {
            agent,
            text: format!("section {index}: {}", "x".repeat(2037)),
            body: None,
            provenance: Some(PeerProvenance::Agent(agent)),
        });
    }
    if info["bare_admission"].as_bool() == Some(true) {
        records.push(Record::MemoryCondenseModelAdmitted {
            agent,
            allowed: true,
            ceiling: None,
            model: info["model"].as_str().unwrap().to_owned(),
        });
    }
    for record in records {
        writer.append(&Entry {
            at: Timestamp::UNIX_EPOCH,
            record,
        })?;
    }
    Ok(())
}

fn snapshot(info: &serde_json::Value) -> anyhow::Result<()> {
    let agent: AgentId = serde_json::from_value(info["agent"].clone())?;
    let journal = PathBuf::from(info["journal"].as_str().unwrap());
    let mut memory = dudes_journal::MemoryProjection::new(journal.parent().unwrap())?;
    let mut records = Vec::new();
    for item in dudes_journal::Reader::new(&journal).iter_from(Lsn::ZERO)? {
        let (lsn, entry) = item?;
        memory.apply_entry(lsn, &entry)?;
        records.push(serde_json::json!({"lsn":lsn,"entry":entry}));
    }
    let raw = memory
        .read(agent, None, 8192)
        .map_err(|size| anyhow::anyhow!("raw page exceeds {size}"))?;
    let summary = memory.summary(agent).map(|summary| {
        serde_json::json!({
            "lsn":summary.lsn,"kind":format!("{:?}",summary.kind),"sections":summary.sections,
            "bytes":summary.bytes,"text":summary.text,
        })
    });
    writeln!(
        std::io::stdout().lock(),
        "{}",
        serde_json::json!({"records":records,"raw":raw,
        "summary":summary,"digest":memory.digest(agent),"launch_recall":memory.work_digest(agent),
        "allowance_consumed":memory.model_condense_already_admitted_this_turn(agent)})
    )?;
    Ok(())
}

#[expect(
    clippy::disallowed_types,
    reason = "the proof child creates one descendant in its inherited owned group so the real runner sweep is observed"
)]
fn descendant() -> std::io::Result<std::process::Child> {
    std::process::Command::new(std::env::current_exe()?)
        .arg("--proof-descendant")
        .spawn()
}
