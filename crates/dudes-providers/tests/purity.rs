//! Source guards for the provider adapter's local purity claims.
//!
//! The adapter is a pure function of `AgentCtx`: `src/claude/managed.rs` says this crate mints no
//! credential and reads no environment of its own, and src/adapter.rs says equal inputs give equal
//! results. This test recursively scans this crate's src/ and keeps two local rules beside the code
//! whose contract they enforce:
//!
//! 1. `FORBIDDEN_ANYWHERE` rejects process, filesystem, input/output, network, thread and clock
//!    work;
//!    reads of this process's environment; and global output sinks. There is no exemption.
//! 2. `ENV_DEBUG_RULE` rejects a `Debug` format spec on a line that names an environment binding.
//!    `EXEMPT` opens that line for rule 2 when the comment states what was redacted.
//!
//! The cross-crate raw-environment representation rule is cargo xtask env-pairs. Keeping it here
//! made a packaged dudes-providers test reach a sibling crate that is absent from the package,
//! while a missing crate row stayed silent.
//!
//! ## Limits
//!
//! These guards make the named accidents loud; they are not a proof of semantic purity. Rule 1 is
//! a substring scan of the spellings in `FORBIDDEN_ANYWHERE`, so an indirect call through a
//! dependency, re-export or macro outside this crate's source is outside its evidence. Rule 2
//! connects an environment-looking token and a Debug spec only when both occur on one source line;
//! it does not interpret format strings or prove that an arbitrary formatter redacts. The exact
//! source list and planted cases below prove the scan read today's files and can fail for every
//! listed spelling. They do not extend either rule beyond those spellings.

use std::fs;
use std::path::{Path, PathBuf};

/// Tokens no provider source may contain, on any line, with no exemption. Comments are included:
/// a comment that needs one of these words should describe the operation without spelling an API.
const FORBIDDEN_ANYWHERE: &[&str] = &[
    "std::process",
    "std::fs",
    "std::io",
    "std::net",
    "std::thread",
    "std::time",
    "std::{",
    "std::env",
    "env::",
    "env!",
    "option_env!",
    "var_os",
    "println!",
    "eprintln!",
    "print!",
    "eprint!",
    "dbg!",
    "tracing::",
    "log::",
];

const DEBUG_SPECS: &[&str] = &[":?}", ":#?}"];
const ENV_DEBUG_RULE: &str = "a Debug format spec on a line that names an environment binding";
const EXEMPT: &str = "purity-ok:";

/// Every rule this text breaks, as (1-based line, what).
fn violations(text: &str) -> Vec<(usize, &'static str)> {
    let mut found = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let at = index + 1;
        found.extend(
            FORBIDDEN_ANYWHERE
                .iter()
                .filter(|token| line.contains(**token))
                .map(|token| (at, *token)),
        );
        if line.contains(EXEMPT) {
            continue;
        }
        if DEBUG_SPECS.iter().any(|spec| line.contains(spec)) && names_an_environment(line) {
            found.push((at, ENV_DEBUG_RULE));
        }
    }
    found
}

/// Whether a line names `env`, `daemon_env`, `env_keys` or another environment binding.
///
/// A following letter excludes envelope and prose containing environment. End of line is a
/// boundary, so a comment ending in env remains visible to the guard.
fn names_an_environment(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.match_indices("env").any(|(at, _)| {
        lower
            .as_bytes()
            .get(at + 3)
            .is_none_or(|next| !next.is_ascii_alphabetic())
    })
}

fn sources(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    collect(dir, &mut out);
    out.sort_by(|(a, _), (b, _)| a.cmp(b));
    out
}

fn collect(dir: &Path, out: &mut Vec<(PathBuf, String)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let text = fs::read_to_string(&path).unwrap();
            out.push((path, text));
        }
    }
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[test]
fn provider_sources_perform_no_io_and_read_no_environment() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let found = sources(&src);
    let scanned: Vec<String> = found.iter().map(|(path, _)| relative(&src, path)).collect();
    assert_eq!(
        scanned,
        [
            "adapter.rs",
            "claude/config.rs",
            "claude/hooks.rs",
            "claude/managed.rs",
            "claude/mod.rs",
            "claude/outcome.rs",
            "claude/stream.rs",
            "claude/text.rs",
            "claude/usage.rs",
            "codex/app_server.rs",
            "codex/config.rs",
            "codex/login.rs",
            "codex/managed.rs",
            "codex/mod.rs",
            "codex/startup.rs",
            "codex/stream.rs",
            "codex/version.rs",
            "instructions.rs",
            "launch.rs",
            "lib.rs",
            "mcp.rs",
            "pi/config.rs",
            "pi/managed.rs",
            "pi/mod.rs",
            "pi/stream.rs",
            "protocol.rs",
            "registry.rs",
            "stream.rs",
        ],
        "the scanned file set changed; add the module and check nothing outside src/ entered"
    );

    for (present, in_file) in [
        ("pub fn scrub_env", "claude/managed.rs"),
        ("pub struct LaunchSpec", "launch.rs"),
    ] {
        let hits: Vec<String> = found
            .iter()
            .filter(|(_, text)| text.contains(present))
            .map(|(path, _)| relative(&src, path))
            .collect();
        assert_eq!(hits, [in_file], "the walk did not read {present}");
    }

    let mut broken = Vec::new();
    for (path, text) in &found {
        for (line, rule) in violations(text) {
            broken.push(format!("{}:{line}: {rule}", relative(&src, path)));
        }
    }
    assert_eq!(
        broken,
        Vec::<String>::new(),
        "the adapter is a pure function of AgentCtx"
    );
}

#[test]
fn the_scan_catches_every_planted_token_through_the_same_walk() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("purity-control");
    let nested = root.join("nested");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&nested).unwrap();
    let cases: &[(&str, &str)] = &[
        ("use std::{fs, io, net, process, thread, time};", "std::{"),
        (
            "let child = std::process::Command::new(\"agent\").spawn()?;",
            "std::process",
        ),
        ("let bytes = std::fs::read(path)?;", "std::fs"),
        ("let sink = std::io::sink();", "std::io"),
        (
            "let stream = std::net::TcpStream::connect(addr)?;",
            "std::net",
        ),
        ("let worker = std::thread::spawn(run);", "std::thread"),
        ("let now = std::time::Instant::now();", "std::time"),
        ("use std::env;", "std::env"),
        ("let key = env::var(\"ANTHROPIC_API_KEY\").ok();", "env::"),
        ("let home = env!(\"HOME\");", "env!"),
        ("let token = option_env!(\"DUDES_TOKEN\");", "option_env!"),
        ("let path = var_os(\"PATH\");", "var_os"),
        ("println!(\"{spec:?}\");", "println!"),
        ("eprintln!(\"{spec:?}\");", "eprintln!"),
        ("print!(\"{spec}\");", "print!"),
        ("eprint!(\"{spec}\");", "eprint!"),
        ("dbg!(&spec);", "dbg!"),
        ("tracing::info!(?spec);", "tracing::"),
        ("log::warn!(\"{spec}\");", "log::"),
        ("write!(f, \"{:?}\", self.env)?;", ENV_DEBUG_RULE),
        ("write!(f, \"{daemon_env:#?}\")?;", ENV_DEBUG_RULE),
        ("write!(f, \"{value:?}\") // from env", ENV_DEBUG_RULE),
        (
            "println!(\"{:?}\", self.env); // purity-ok: redacted",
            "println!",
        ),
    ];
    for (index, (line, _)) in cases.iter().enumerate() {
        fs::write(nested.join(format!("case_{index:02}.rs")), line).unwrap();
    }
    let found = sources(&root);
    assert_eq!(found.len(), cases.len(), "the walk skipped a planted file");
    for ((path, text), (line, expected)) in found.iter().zip(cases) {
        assert!(
            violations(text).contains(&(1, *expected)),
            "the walk did not catch {expected} in {line} at {}",
            path.display()
        );
    }

    assert_eq!(
        FORBIDDEN_ANYWHERE.len(),
        19,
        "add a planted case when the forbidden list changes"
    );
    assert_eq!(
        DEBUG_SPECS.len(),
        2,
        "add a planted case when the Debug spec list changes"
    );
}

#[test]
fn the_scan_leaves_local_safe_shapes_alone() {
    let cases = [
        "pub fn scrub_env(daemon_env: &[(OsString, OsString)]) -> Vec<(OsString, OsString)> {}",
        "let case = format!(\"{subtype}/{reason:?}/{exit:?}\");",
        "panic!(\"{envelope:?}\");",
        "f.debug_struct(\"LaunchSpec\").field(\"env\", &Redacted(self.env.len()))",
        "write!(f, \"{env_keys:?}\")?; // purity-ok: keys only, every value dropped",
    ];
    for line in cases {
        assert_eq!(
            violations(line),
            [],
            "a safe provider-source shape was flagged: {line}"
        );
    }
}
