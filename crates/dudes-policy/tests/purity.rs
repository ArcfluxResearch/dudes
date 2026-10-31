//! Invariant 5 (the loop is pure): nothing under `src/` names a clock, the file system, the
//! network, a process, a thread or mutable global state. The layering gate keeps crates that act
//! out of the dependency graph; this scan keeps the standard library's own side effects out of
//! the source. It walks `src/` recursively, so a future `src/gates/*.rs` is scanned too. The scan
//! itself reads files — that is why it lives in `tests/`, which it does not scan.

use std::fs;
use std::path::{Path, PathBuf};

/// Tokens no policy source may contain. Matched as substrings, comments included: a doc comment
/// that needs one of these words should say "clock" or "I/O" instead. `std::{` and `::std` catch
/// the grouped and the fully qualified forms (`use std::{fs, io}`, `::std::time::Instant`).
const FORBIDDEN: &[&str] = &[
    // `at` labels a record, it is never an input to `decide()`, so
    // `LoopInput` carries no wall time and no day bucket derived from one.
    "Timestamp",
    "UtcDay",
    "std::time",
    "std::fs",
    "std::io",
    "std::net",
    "std::process",
    "std::thread",
    "std::env",
    "std::sync",
    "std::{",
    "::std",
    "Instant",
    "SystemTime",
    "static mut",
    "thread_local!",
    "println!",
    "eprintln!",
    "dbg!",
];

/// The forbidden tokens `text` contains. The one place the rule is applied, so the scan below
/// and the self-test that proves the scan works cannot drift apart.
fn violations(text: &str) -> Vec<&'static str> {
    FORBIDDEN
        .iter()
        .copied()
        .filter(|token| text.contains(token))
        .collect()
}

/// Every `.rs` file under `dir` at any depth, as `(path, contents)`, sorted by path. The one code
/// path from a directory to text: the control below plants files in a temporary directory and runs
/// *this* function over them, so it exercises the walk and the read, not only [`violations`].
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
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let text = fs::read_to_string(&path).unwrap();
            out.push((path, text));
        }
    }
}

/// `path` relative to `root`, with `/` separators on every platform, for stable assertions.
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap()
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Invariant 5: every `.rs` file under `src/`, at any depth, is free of the forbidden tokens.
#[test]
fn policy_sources_name_no_clock_and_no_io() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let sources = sources(&src);

    // An absence is meaningful only over the corpus named here. Exact, rather than a lower bound:
    // a missed module and a swept-in directory both change the result. `purity_witness/mod.rs` is
    // a live recursive witness, so removing the walk's recursive arm makes this assertion fail.
    let scanned: Vec<String> = sources.iter().map(|(path, _)| rel(&src, path)).collect();
    assert_eq!(
        scanned,
        [
            "breaker.rs",
            "budget.rs",
            "chat.rs",
            "closing.rs",
            "input.rs",
            "lib.rs",
            "loop.rs",
            "mail.rs",
            "mail_turn.rs",
            "predicate.rs",
            "proptest.rs",
            "purity_witness/mod.rs",
        ],
        "the scanned file set changed; add the new module here, and check nothing outside src/ crept in"
    );

    for (path, text) in &sources {
        let found = violations(text);
        assert!(
            found.is_empty(),
            "{} mentions {found:?}; the loop is pure (invariant 5)",
            path.display()
        );
    }
}

/// The scan above passes by finding nothing, which proves nothing unless the scan can fail:
/// a typo in [`FORBIDDEN`] ("Timestmap") would sit there green forever. This plants each class in
/// real `.rs` files two levels down and runs [`sources`] — the same recursive walk, read and matcher
/// the assertion above uses — over them. The count assertion fails when a token is added without a
/// planted case for it.
#[test]
fn the_scan_catches_planted_tokens_through_the_same_walk() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("policy-purity-control");
    let planted = root.join("nested");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&planted).unwrap();

    let cases: &[(&str, &str)] = &[
        ("let now = std::time::Instant::now();", "std::time"),
        ("use std::fs;", "std::fs"),
        ("let stamp: Timestamp = input.at;", "Timestamp"),
        ("fn bucket(day: UtcDay) -> u32 { 0 }", "UtcDay"),
        ("static mut SEEN: u8 = 0;", "static mut"),
        ("println!(\"{decision:?}\");", "println!"),
    ];
    for (index, (line, _)) in cases.iter().enumerate() {
        fs::write(planted.join(format!("case_{index:02}.rs")), line).unwrap();
    }

    let found = sources(&root);
    assert_eq!(
        found.len(),
        cases.len(),
        "the control planted {} files and the walk read {}",
        cases.len(),
        found.len()
    );
    for ((path, text), (line, expected)) in found.iter().zip(cases) {
        // `contains(&expected)`, not `!is_empty()`: with overlapping tokens the wrong entry could
        // otherwise answer for this one, and the case would prove nothing about `expected`.
        assert!(
            violations(text).contains(expected),
            "the walk did not catch `{expected}` in `{line}` at {}",
            path.display()
        );
    }

    let clean = root.join("clean");
    fs::create_dir_all(&clean).unwrap();
    fs::write(
        clean.join("quiet.rs"),
        "pub fn decide(input: &LoopInput) -> LoopDecision { gate_control(input) }",
    )
    .unwrap();
    assert_eq!(
        sources(&clean)
            .iter()
            .flat_map(|(_, text)| violations(text))
            .collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "a clean source read through the walk must not be flagged"
    );
    // A tripwire, not a coverage claim: the planted cases above prove six of the tokens, so a
    // change to the list is a prompt to check whether the new entry needs one of its own.
    assert_eq!(
        FORBIDDEN.len(),
        19,
        "the forbidden list changed; check whether the new entry needs a planted case above"
    );
}
