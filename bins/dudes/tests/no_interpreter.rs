//! Invariant 7's other half: the CLI names no interpreter route and composes no shell string, and
//! the scan that says so fires.
//!
//! The CLI's four process items start quickstart's daemon, an engine's login, the
//! operator's selected editor, or `dudes upgrade`'s system `curl` and release installer. Those programs may themselves be scripts. This file confines
//! what Dudes composes: typed argv, an absolute program, and no shell string. Only the editor
//! resolves a launch program on the operator's `PATH`.
//!
//! # Why a scan when `clippy.toml` already bans the type
//!
//! `bins/dudes/clippy.toml` disallows both admitted command types, which is stronger than a scan in
//! one way and weaker in three:
//!
//! - **Stronger**: it is a compile error under `-D warnings`, so it cannot be merged past.
//! - **Weaker**: it bans a *type*. Another crate's spawn API — `xshell`, `duct`, a `libc` call — is
//!   not that type and passes.
//! - **Weaker**: it can be answered with `#[allow]`. `tests/common/mod.rs` answers it once with
//!   `#[expect]`, on
//!   purpose, because an exit code is a property of a process. A scan over `src/` catches a
//!   `Command` there *whatever attribute sits above it*.
//! - **Weaker**: it says nothing about a *shell*. `"sh"`, `"bash"` and `-c` are ordinary strings.
//!
//! # What defends this package
//!
//! Creating a process does not have to go through `process::Command`, `execve` or `execvp`.
//! Planting eight routes and running the shipped [`hits`] over them shows it: `libc::system` is
//! `/bin/sh -c` under another name, `libc::posix_spawnp` creates a process, and neither names any of those
//! three. Two more — `xshell::cmd!` and a `shell_out` helper — are other crates' APIs that the
//! clippy type ban cannot see either.
//!
//! So the honest statement of what defends this package is three partial things, none complete:
//!
//! 1. **The clippy type ban**, which catches `std::process::Command` including a glob-imported or
//!    renamed one, and nothing else.
//! 2. **This scan**, which catches the routes named in [`CODE_ROUTES`] and [`QUOTED_ROUTES`] —
//!    including the four the type ban misses — and cannot see a spawn hidden behind a name it does
//!    not know.
//! 3. **The dependency list**, which is a visible diff line rather than a gate. `cargo xtask
//!    layering`'s `dudes` row is **not** a complement: it is a denylist of fourteen named crates,
//!    none of them a spawn crate. And `libc` cannot be added to it, because `dudes` reaches `libc`
//!    transitively through `dudes → dudes-rpc → nix`, so a row banning it would fail on the
//!    shipped graph.
//!
//! The complete gate would be a `cargo deny` ban entry. That residual is stated here rather than
//! papered over.
//!
//! # The false positive that took the `shell` needle out, and how it is solved now
//!
//! `shell` was removed once because it fired on `src/args.rs`'s `USAGE` constant — the sentence
//! explaining that nothing is handed to a shell. Skipping lines that *start* with `//` did not help:
//! a usage string is neither a comment nor a call, and `USAGE` is a multi-line literal whose inner
//! lines carry no quote character at all.
//!
//! It is solved by **masking** rather than by dropping the needle. [`masked`] walks the file once,
//! tracking whether it is inside a line comment and inside a string literal, and returns two texts:
//! one with comments blanked, one with comments *and* string contents blanked. A needle that is
//! itself a string literal is matched against the first; a needle that is an identifier is matched
//! against the second. Line numbers survive, so a hit still reports where it is.
//!
//! That is strictly stronger than the old prefix skip, which also could not see a trailing `// …`
//! comment on a line of code.
//!
//! # The scan's own control
//!
//! **A search that returns nothing proves nothing until the same search has been shown to return
//! something.** [`a_planted_interpreter_is_caught`] feeds [`hits`] — the same function, not a
//! re-derivation of it — a tree containing every route above, including the five that a narrower
//! needle set would let through, and asserts every one is reported. And [`hits`] refuses
//! a root it cannot read or one with fewer than [`MIN_FILES`] files, so a scan whose directory has
//! moved fails rather than reading as clean.
use std::path::{Path, PathBuf};

/// Routes matched against the line with **comments and string literals blanked**.
///
/// Each is an identifier or a path, so it must not be matched inside prose. `process::` on its own
/// is deliberately absent: `src/main.rs` legitimately writes `use std::process::ExitCode;`, and a
/// needle that fired on it would be the false positive this file exists to avoid.
const CODE_ROUTES: &[&str] = &[
    // `std::process::Command`, `tokio::process::Command`, and `use std::process::Command as Runner`
    // — all three name this path on the line that introduces them.
    "process::Command",
    // The Windows-only command route is checked from source on every host.
    "windows_spawn::Command",
    // A glob import hides the type name at every later call site, so the import is the only place
    // it can be seen.
    "process::*",
    "pre_exec",
    "execve",
    "execvp",
    // `posix_spawn` and `posix_spawnp`.
    "posix_spawn",
    // `libc::system` is `/bin/sh -c` under another name. Written with the `::` and the paren so it
    // cannot fire on a field, a variable or the ordinary English word.
    "::system(",
    // The word itself, which is what catches another crate's shell API: `xshell::cmd!`, a
    // `shell_out` helper, or a `shell` binding handed to `Command::new`. Safe to match only because
    // string literals are blanked first — this is the needle whose false positive on `USAGE`
    // caused it to be dropped, and masking is what brings it back.
    "shell",
];

/// Routes matched against the line with **only comments blanked**, because each one *is* a string
/// literal and blanking strings would erase it.
const QUOTED_ROUTES: &[&str] = &["\"sh\"", "\"bash\"", "\"/bin/sh\"", "\"-c\""];

/// Under the floor the scan refuses to believe it read anything.
///
/// A walk that saw fewer than six files has lost a directory, and a zero-hit result from a walk that
/// read nothing is the failure this constant is for.
const MIN_FILES: usize = 6;

/// The files that may name a command type, each with one spawn item.
const ROUTE_FILES: [&str; 5] = [
    "quickstart.rs",
    "login.rs",
    "editor.rs",
    "runner.rs",
    "opener.rs",
];

/// The one interpreter `dudes upgrade` runs: the release's `install.sh`, as `/bin/sh <file> ...`.
/// `src/upgrade.rs` names it exactly once, and its test module may repeat the path to assert the
/// argument vector. Every other file naming an interpreter is still refused.
const INSTALLER_INTERPRETER: &str = "\"/bin/sh\"";
const INSTALLER_FILES: [&str; 2] = ["upgrade.rs", "upgrade/tests.rs"];

/// The lints whose `allow` or `expect` silences the command-type ban: the two lints it is made of,
/// the clippy groups that hold them, and every warning at once.
const BAN_LINTS: [&str; 5] = [
    "clippy::disallowed_types",
    "clippy::disallowed_methods",
    "clippy::style",
    "clippy::all",
    "warnings",
];

/// The files of this package that may carry one `#[expect(clippy::disallowed_types)]`, and nothing
/// else from [`BAN_LINTS`]: the three route items, and the two test helpers that start a real
/// process (`bins/dudes/clippy.toml` item 3).
const ADMITTED_EXPECTATIONS: [&str; 7] = [
    "src/quickstart.rs",
    "src/login.rs",
    "src/editor.rs",
    "src/runner.rs",
    "src/opener.rs",
    "tests/common/mod.rs",
    "tests/windows_pipe_owner.rs",
];

/// The two command types the scan confines rather than forbids.
const COMMAND_ROUTES: [&str; 2] = ["process::Command", "windows_spawn::Command"];

/// Blank comments, and blank string contents, in one pass.
///
/// Returns `(without_comments, without_comments_or_strings)`. Both keep every newline and every
/// character position, so a line number in one is a line number in the source.
///
/// # What this does not handle, stated rather than assumed
///
/// Block comments (`/* … */`) and raw strings (`r"…"`, `r#"…"#`). Neither appears anywhere under
/// `bins/dudes/`, and both would need a real lexer to do properly. A raw string containing one of
/// [`CODE_ROUTES`] would be reported as a hit — which is the safe direction to be wrong in, because
/// it fails loudly and a reader can look.
fn masked(text: &str) -> (String, String) {
    let mut no_comments = String::with_capacity(text.len());
    let mut code_only = String::with_capacity(text.len());
    let mut in_string = false;
    let mut in_comment = false;
    let mut escaped = false;
    let mut chars = text.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\n' {
            in_comment = false;
            // A string literal spans lines — `USAGE` is one — so `in_string` deliberately survives
            // the newline. That is the whole reason the old per-line skip could not see it.
            no_comments.push('\n');
            code_only.push('\n');
            continue;
        }
        if in_comment {
            no_comments.push(' ');
            code_only.push(' ');
            continue;
        }
        if in_string {
            no_comments.push(ch);
            code_only.push(' ');
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            in_comment = true;
            no_comments.push(' ');
            code_only.push(' ');
            continue;
        }
        if ch == '"' {
            in_string = true;
            escaped = false;
            no_comments.push(ch);
            code_only.push(' ');
            continue;
        }
        no_comments.push(ch);
        code_only.push(ch);
    }
    (no_comments, code_only)
}

/// Every `(file, line number, route)` in `root` that carries a route to a child process or an
/// interpreter.
///
/// # Panics
///
/// If `root` cannot be walked or holds fewer than [`MIN_FILES`] files — a walk that has stopped
/// seeing files must fail rather than read as clean.
#[derive(Debug)]
struct Hit {
    path: PathBuf,
    line: usize,
    route: &'static str,
    text: String,
}

fn hits(root: &Path) -> Vec<Hit> {
    let mut found = Vec::new();
    let mut files = 0usize;
    walk(root, &mut |path| {
        if path.extension().is_none_or(|ext| ext != "rs") {
            return;
        }
        files += 1;
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
        let (no_comments, code_only) = masked(&text);
        for (number, ((raw, quoted), code)) in text
            .lines()
            .zip(no_comments.lines())
            .zip(code_only.lines())
            .enumerate()
        {
            let mut report = |route: &'static str| {
                found.push(Hit {
                    path: path.to_path_buf(),
                    line: number + 1,
                    route,
                    text: raw.trim().to_owned(),
                });
            };
            for route in QUOTED_ROUTES {
                if quoted.contains(route) {
                    report(route);
                }
            }
            for route in CODE_ROUTES {
                if code.contains(route) {
                    report(route);
                }
            }
        }
    });
    assert!(
        files >= MIN_FILES,
        "the scan read {files} files under {}, which is fewer than {MIN_FILES}: the walk has \
         stopped seeing the tree and a zero-hit result would mean nothing",
        root.display()
    );
    found
}

fn format_hits(found: &[&Hit]) -> String {
    found
        .iter()
        .map(|hit| {
            format!(
                "{}:{}: {} — {}",
                hit.path.display(),
                hit.line,
                hit.route,
                hit.text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Each `allow(…)` or `expect(…)` in `text` that names one of [`BAN_LINTS`], as `kind(lint)`.
///
/// Read over comment- and string-blanked, whitespace-collapsed code, so neither prose nor rustfmt's
/// wrapping decides the answer, and a `cfg_attr` or an inner `#![…]` is read like any other.
fn ban_suppressions(text: &str) -> Vec<String> {
    let (_, code) = masked(text);
    let collapsed: String = code.split_whitespace().collect();
    let mut found = Vec::new();
    for kind in ["allow(", "expect("] {
        let mut rest = collapsed.as_str();
        while let Some(at) = rest.find(kind) {
            rest = &rest[at + kind.len()..];
            let body = &rest[..rest.find(')').unwrap_or(rest.len())];
            for lint in body.split(',') {
                if BAN_LINTS.contains(&lint) {
                    found.push(format!("{kind}{lint})"));
                }
            }
        }
    }
    found
}

/// What keeps the suppression scan honest: every file of `package` carries exactly its
/// admitted `#[expect(clippy::disallowed_types)]` and no other suppression of the ban, and `src/`
/// pulls in no file through `#[path]` or `include!`.
fn attribute_violations(package: &Path) -> Vec<String> {
    let mut violations = Vec::new();
    for dir in ["src", "tests"] {
        walk(&package.join(dir), &mut |path| {
            if path.extension().is_none_or(|ext| ext != "rs") {
                return;
            }
            let file = path
                .strip_prefix(package)
                .unwrap_or(path)
                .components()
                .map(|part| part.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let text = std::fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
            let found = ban_suppressions(&text);
            let admitted: Vec<String> = if ADMITTED_EXPECTATIONS.contains(&file.as_str()) {
                vec!["expect(clippy::disallowed_types)".to_owned()]
            } else {
                Vec::new()
            };
            if found != admitted {
                let shown = if found.is_empty() {
                    "none".to_owned()
                } else {
                    found.join(", ")
                };
                violations.push(format!("(c) {file} carries {shown}"));
            }
            if dir == "src" {
                let (_, code) = masked(&text);
                let collapsed: String = code.split_whitespace().collect();
                if collapsed.contains("#[path=") || collapsed.contains("#![path=") {
                    violations.push(format!("(d) {file} has a #[path] module"));
                }
                if collapsed.contains("include!(") {
                    violations.push(format!("(d) {file} uses include!"));
                }
            }
        });
    }
    violations
}

/// Every way `package` breaks the assertions, one line each, sorted.
fn route_violations(package: &Path) -> Vec<String> {
    let src = package.join("src");
    let found = hits(&src);
    let name = |path: &Path| format!("src/{}", path.strip_prefix(&src).unwrap_or(path).display());
    let mut violations = Vec::new();
    for hit in &found {
        if hit.route == INSTALLER_INTERPRETER
            && INSTALLER_FILES
                .iter()
                .any(|file| hit.path == src.join(file))
        {
            continue;
        }
        if !COMMAND_ROUTES.contains(&hit.route) {
            violations.push(format!(
                "(a) {}:{} {}",
                name(&hit.path),
                hit.line,
                hit.route
            ));
        } else if !ROUTE_FILES.iter().any(|file| hit.path == src.join(file)) {
            violations.push(format!(
                "(b) {}:{} {}",
                name(&hit.path),
                hit.line,
                hit.route
            ));
        }
    }
    for (file, routes) in [
        ("quickstart.rs", &COMMAND_ROUTES[..]),
        ("login.rs", &COMMAND_ROUTES[..1]),
        ("editor.rs", &COMMAND_ROUTES[..1]),
        ("runner.rs", &COMMAND_ROUTES[..1]),
        ("opener.rs", &COMMAND_ROUTES[..1]),
    ] {
        for route in routes {
            if !found
                .iter()
                .any(|hit| hit.path == src.join(file) && hit.route == *route)
            {
                violations.push(format!("(b') src/{file} carries no {route}"));
            }
        }
    }
    let installer = found
        .iter()
        .filter(|hit| hit.route == INSTALLER_INTERPRETER && hit.path == src.join("upgrade.rs"))
        .count();
    if installer != 1 {
        violations.push(format!(
            "(e) src/upgrade.rs names {INSTALLER_INTERPRETER} {installer} times, not once"
        ));
    }
    violations.extend(attribute_violations(package));
    violations.sort();
    violations
}

fn walk(dir: &Path, visit: &mut impl FnMut(&Path)) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|error| panic!("reading {}: {error}", dir.display()));
    for entry in entries {
        let path = entry.expect("a directory entry").path();
        if path.is_dir() {
            walk(&path, visit);
        } else {
            visit(&path);
        }
    }
}

/// The source names no interpreter route, confines both command types to the two route files,
/// and carries only the admitted lint expectations, with no other suppression of the ban anywhere
/// in the package.
#[test]
fn the_cli_composes_no_shell_and_confines_its_routes() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"));
    assert_eq!(route_violations(package), Vec::<String>::new());
}

/// A copy of this package's `src/` and `tests/` in a temporary tree, with `edit` applied to it.
fn mutated_package(edit: impl FnOnce(&Path)) -> tempfile::TempDir {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("a directory");
        for entry in std::fs::read_dir(from).expect("the source tree") {
            let path = entry.expect("an entry").path();
            let target = to.join(path.file_name().expect("a name"));
            if path.is_dir() {
                copy(&path, &target);
            } else {
                std::fs::copy(&path, &target).expect("a source file");
            }
        }
    }
    let tree = tempfile::tempdir().expect("a temporary tree");
    for dir in ["src", "tests"] {
        copy(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(dir),
            &tree.path().join(dir),
        );
    }
    edit(tree.path());
    tree
}

fn replace_in(path: &Path, from: &str, to: &str) {
    let text = std::fs::read_to_string(path).expect("a source file");
    assert_eq!(
        text.matches(from).count(),
        1,
        "the mutation target drifted in {}",
        path.display()
    );
    std::fs::write(path, text.replacen(from, to, 1)).expect("write the mutant");
}

fn append_to(path: &Path, tail: &str) {
    let text = std::fs::read_to_string(path).expect("a source file");
    std::fs::write(path, format!("{text}\n{tail}\n")).expect("write the mutant");
}

/// The named mutants of the scan, each red for its own reason on a copy of the package.
#[test]
fn the_route_mutants_fail_as_designed() {
    let fourth = mutated_package(|package| {
        std::fs::write(
            package.join("src/fourth.rs"),
            "fn go() {\n    let _ = std::process::Command::new(\"/usr/bin/true\");\n}\n",
        )
        .expect("plant a fourth route");
    });
    assert_eq!(
        route_violations(fourth.path()),
        vec!["(b) src/fourth.rs:2 process::Command"],
        "a command in a fourth file fails (b)"
    );

    let second = mutated_package(|package| {
        append_to(
            &package.join("src/login.rs"),
            "#[expect(clippy::disallowed_types, reason = \"a second\")]\nfn second() {}",
        );
    });
    assert_eq!(
        route_violations(second.path()),
        vec![
            "(c) src/login.rs carries expect(clippy::disallowed_types), \
             expect(clippy::disallowed_types)"
        ],
        "a second expectation in src/login.rs fails (c)"
    );

    let allowed = mutated_package(|package| {
        replace_in(
            &package.join("src/login.rs"),
            "#[expect(\n    clippy::disallowed_types,",
            "#[allow(\n    clippy::disallowed_types,",
        );
    });
    assert_eq!(
        route_violations(allowed.path()),
        vec!["(c) src/login.rs carries allow(clippy::disallowed_types)"],
        "allow in place of expect fails (c)"
    );

    let emptied = mutated_package(|package| {
        std::fs::write(package.join("src/login.rs"), "//! Nothing here.\n")
            .expect("empty the login");
    });
    assert_eq!(
        route_violations(emptied.path()),
        vec![
            "(b') src/login.rs carries no process::Command",
            "(c) src/login.rs carries none",
        ],
        "a route file with no route leaves (b) vacuous, which (b') refuses"
    );

    let shell = mutated_package(|package| {
        append_to(
            &package.join("src/login.rs"),
            "const SHELL: &str = \"/bin/sh\";",
        );
    });
    assert_eq!(
        route_violations(shell.path()),
        vec![format!(
            "(a) src/login.rs:{} \"/bin/sh\"",
            std::fs::read_to_string(shell.path().join("src/login.rs"))
                .expect("the mutant")
                .lines()
                .position(|line| line.contains("\"/bin/sh\""))
                .expect("the planted line")
                + 1
        )],
        "an interpreter inside a route file still fails (a)"
    );
}

/// `dudes upgrade` may name its installer's interpreter once, and nothing else may spawn or interpret.
#[test]
fn the_upgrade_interpreter_is_confined_to_one_spelling() {
    let twice = mutated_package(|package| {
        append_to(
            &package.join("src/upgrade.rs"),
            "const AGAIN: &str = \"/bin/sh\";",
        );
    });
    assert_eq!(
        route_violations(twice.path()),
        vec!["(e) src/upgrade.rs names \"/bin/sh\" 2 times, not once"],
        "a second interpreter spelling in the upgrade file fails (e)"
    );

    let bash = mutated_package(|package| {
        append_to(
            &package.join("src/upgrade.rs"),
            "const OTHER: &str = \"bash\";",
        );
    });
    assert_eq!(
        route_violations(bash.path()).len(),
        1,
        "any other interpreter in the upgrade file fails (a)"
    );

    let spawn = mutated_package(|package| {
        append_to(
            &package.join("src/upgrade.rs"),
            "fn go() {\n    let _ = std::process::Command::new(\"/usr/bin/true\");\n}",
        );
    });
    assert!(
        route_violations(spawn.path())
            .iter()
            .any(|line| line.starts_with("(b) src/upgrade.rs:")),
        "a command in the upgrade file fails (b): only runner.rs may spawn"
    );
}

/// A group-level or method-ban suppression hides a second spawn from clippy, so (c) refuses every
/// one of them anywhere in the package, and `src/` may pull in no file through `#[path]` or
/// `include!`, which would put code outside the scanned tree.
#[test]
fn every_other_suppression_of_the_ban_is_refused() {
    type Edit = Box<dyn FnOnce(&Path)>;
    const SECOND_SPAWN: &str =
        "fn second() {\n    let _ = std::process::Command::new(\"/usr/bin/osascript\").spawn();\n}";
    let arms: Vec<(Edit, Vec<&str>)> = vec![
        (
            Box::new(|package: &Path| {
                append_to(
                    &package.join("src/login.rs"),
                    &format!("#[allow(clippy::style)]\n{SECOND_SPAWN}"),
                );
            }),
            vec!["(c) src/login.rs carries allow(clippy::style), expect(clippy::disallowed_types)"],
        ),
        (
            Box::new(|package: &Path| {
                let login = package.join("src/login.rs");
                let text = std::fs::read_to_string(&login).expect("the login");
                std::fs::write(&login, format!("#![allow(clippy::all)]\n{text}"))
                    .expect("write the mutant");
            }),
            vec!["(c) src/login.rs carries allow(clippy::all), expect(clippy::disallowed_types)"],
        ),
        (
            Box::new(|package: &Path| {
                append_to(
                    &package.join("src/quickstart.rs"),
                    &format!("#[expect(clippy::disallowed_methods)]\n{SECOND_SPAWN}"),
                );
            }),
            vec![
                "(c) src/quickstart.rs carries expect(clippy::disallowed_types), \
                 expect(clippy::disallowed_methods)",
            ],
        ),
        (
            Box::new(|package: &Path| {
                append_to(
                    &package.join("src/login.rs"),
                    &format!("#[cfg_attr(unix, allow(warnings))]\n{SECOND_SPAWN}"),
                );
            }),
            vec!["(c) src/login.rs carries allow(warnings), expect(clippy::disallowed_types)"],
        ),
        (
            Box::new(|package: &Path| {
                append_to(
                    &package.join("tests/doors.rs"),
                    "#[allow(clippy::disallowed_types)]\nfn helper() {}",
                );
            }),
            vec!["(c) tests/doors.rs carries allow(clippy::disallowed_types)"],
        ),
        (
            Box::new(|package: &Path| {
                append_to(
                    &package.join("src/lib.rs"),
                    "#[path = \"../../outside.rs\"]\nmod outside;",
                );
            }),
            vec!["(d) src/lib.rs has a #[path] module"],
        ),
        (
            Box::new(|package: &Path| {
                append_to(
                    &package.join("src/run.rs"),
                    "include!(\"../../outside.rs\");",
                );
            }),
            vec!["(d) src/run.rs uses include!"],
        ),
    ];
    for (index, (edit, expected)) in arms.into_iter().enumerate() {
        let tree = mutated_package(edit);
        assert_eq!(
            route_violations(tree.path()),
            expected,
            "suppression arm {index}"
        );
    }
}

/// **The control.** The same function, over a tree with every route planted, reports each one.
///
/// The five marked below are the ones a narrower needle set would let through. They are the reason
/// this test exists in this shape: without them it would go on asserting that a scan catches only
/// the obvious routes.
#[test]
fn a_planted_interpreter_is_caught() {
    let planted = tempfile::tempdir().expect("a temporary tree");
    // A subdirectory as well as the root, so the walk's recursion is exercised rather than assumed.
    let nested = planted.path().join("nested");
    std::fs::create_dir(&nested).expect("a subdirectory");

    let plants: &[(&str, &str)] = &[
        // --- the obvious routes ----------------------------------------------------------------
        ("a.rs", "let mut c = std::process::Command::new(\"true\");"),
        ("b.rs", "use tokio::process::Command;"),
        ("c.rs", "unsafe { command.pre_exec(|| Ok(())) };"),
        ("d.rs", "libc::execve(path, argv, envp);"),
        ("e.rs", "libc::execvp(path, argv);"),
        // Split for the same reason as `i.rs`/`i2.rs`: together on one line, `"sh"` alone reported
        // the file and `"-c"` was never shown to fire.
        ("f.rs", "let argv = [\"sh\", line];"),
        ("f2.rs", "let argv = [prog, \"-c\", line];"),
        ("g.rs", "let argv = [\"bash\", \"-lc\", line];"),
        ("h.rs", "let argv = [\"/bin/sh\", line];"),
        // --- the five a narrower needle set misses -------------------------------------------------
        // **One route per file, and that is load-bearing.** The assertion below is per file, so a
        // plant carrying two routes is reported when *either* needle fires — which means removing
        // one of them leaves this test green and the needle unproven. With the glob import
        // and the `Command::new(shell)` call in one file, deleting the `process::*` needle would not
        // redden anything. Split, each needle is the only thing standing between its plant and a
        // miss.
        ("i.rs", "use std::process::*;"),
        ("i2.rs", "let c = Command::new(shell).arg(flag);"),
        // Another crate's shell API.
        ("j.rs", "xshell::cmd!(sh, \"rm -rf {dir}\").run()?;"),
        // A helper whose name is the only clue.
        ("k.rs", "shellwords_exec::shell_out(line)?;"),
        // `/bin/sh -c` under another name.
        ("l.rs", "unsafe { libc::system(c) };"),
        // Creates a process and names none of `process::Command`, `execve` or `execvp`.
        (
            "m.rs",
            "unsafe { libc::posix_spawnp(&mut pid, prog, ptr, ptr, argv, envp) };",
        ),
        // A rename, which the type ban catches and which must not be lost from the scan either.
        ("n.rs", "use std::process::Command as Runner;"),
    ];
    for (index, (name, line)) in plants.iter().enumerate() {
        let dir = if index % 2 == 0 {
            planted.path()
        } else {
            nested.as_path()
        };
        std::fs::write(dir.join(name), format!("// a planted route\n{line}\n"))
            .expect("plant a route");
    }

    let found = hits(planted.path());
    for (name, line) in plants {
        assert!(
            found.iter().any(|hit| hit.path.ends_with(name)),
            "the scan missed the route planted in {name} (`{line}`); it found:\n{}",
            format_hits(&found.iter().collect::<Vec<_>>())
        );
    }
}

/// A route in a comment is not a hit; a route in a **string literal** is not a hit; a route in code
/// is.
///
/// The three arms together are what the masking buys, and the third is the control: without it a
/// scan that had stopped reading files at all would pass the two absences.
///
/// The string arm is the exact false positive that once caused the `shell` needle to be deleted —
/// `src/args.rs`'s `USAGE` constant is a multi-line literal whose inner lines carry no quote
/// character, so a per-line skip could not see it. It is reproduced here in that shape.
#[test]
fn a_route_in_a_comment_or_a_string_is_not_a_hit_but_one_in_code_is() {
    let tree = tempfile::tempdir().expect("a temporary tree");
    // `MIN_FILES` files exactly, so the first `hits` call below is a scan the floor admits: a
    // planted tree that sat under the floor would fail on the floor rather than on the property.
    for name in ["o.rs", "p.rs", "q.rs", "r.rs", "s.rs", "t.rs"] {
        std::fs::write(
            tree.path().join(name),
            "//! Nothing here creates a process; `std::process::Command` is banned.\n\
             // and `sh -c` is never built, nor is libc::system(x).\n\
             fn ordinary() {}\n",
        )
        .expect("write a commented file");
    }
    assert!(
        hits(tree.path()).is_empty(),
        "a route inside a comment is prose, not a call"
    );

    // A multi-line string literal in `USAGE`'s own shape: the word sits on a line with no quote on
    // it at all, so only cross-line string tracking can see that it is inside a literal.
    std::fs::write(
        tree.path().join("usage.rs"),
        "pub const USAGE: &str = \"\\\n\
         COMMANDS\n\
             goal -- <program> [args...]\n\
                 A typed list resolved by the daemon, never a shell string.\n\
         \";\n",
    )
    .expect("write a usage-shaped file");
    assert!(
        hits(tree.path()).is_empty(),
        "a route inside a multi-line string literal is prose, not a call"
    );

    std::fs::write(
        tree.path().join("live.rs"),
        "//! But this file really does it.\nfn go() { std::process::Command::new(\"x\"); }\n",
    )
    .expect("write a live file");
    let found = hits(tree.path());
    assert_eq!(
        found.len(),
        1,
        "control: exactly the live one is reported:\n{}",
        format_hits(&found.iter().collect::<Vec<_>>())
    );
    assert!(found[0].path.ends_with("live.rs"), "{found:?}");
}

/// A trailing comment on a line of code is masked, and the code before it is not.
///
/// The prefix skip this replaced could only see a comment that *began* a line, so
/// `let x = 1; // std::process::Command` was a hit and `Command::new(sh) // fine` was not caught by
/// anything. Both arms, because either alone is satisfied by a scan that ignores comments entirely.
#[test]
fn a_trailing_comment_is_masked_and_the_code_before_it_is_not() {
    let tree = tempfile::tempdir().expect("a temporary tree");
    // Five, plus the live `y.rs` below: `MIN_FILES` in total, for the reason the sibling test gives.
    for name in ["t.rs", "u.rs", "v.rs", "w.rs", "x.rs"] {
        std::fs::write(
            tree.path().join(name),
            "fn ordinary() { let _x = 1; } // std::process::Command is banned here\n",
        )
        .expect("write a file with a trailing comment");
    }
    std::fs::write(
        tree.path().join("y.rs"),
        "fn go() { let _ = xshell::cmd!(sh, \"ls\"); } // this half is prose\n",
    )
    .expect("write a live file with a trailing comment");

    let found = hits(tree.path());
    assert_eq!(
        found.len(),
        1,
        "the trailing comments are masked and the live call is not:\n{}",
        format_hits(&found.iter().collect::<Vec<_>>())
    );
    assert!(found[0].path.ends_with("y.rs"), "{found:?}");
}

/// The suppression scan sees the wrapped form rustfmt produces, ignores a comment or a string
/// carrying the same words, and reads a `cfg_attr` and an inner attribute like any other.
#[test]
fn wrapped_lint_expectations_are_counted_after_masking() {
    assert_eq!(
        ban_suppressions(
            "// #[allow(clippy::disallowed_types)] is prose\n\
             const NOTE: &str = \"#[allow(clippy::style)]\";\n\
             #[expect(\n    clippy::disallowed_types,\n    reason = \"a reason\"\n)]\n\
             fn start() {}\n"
        ),
        ["expect(clippy::disallowed_types)"]
    );
    assert_eq!(
        ban_suppressions(
            "#![allow(\n    clippy::all\n)]\n#[cfg_attr(unix, allow(clippy::style, dead_code))]\n\
             #[allow(\n    clippy::disallowed_types,\n    reason = \"mutant\"\n)]\nfn start() {}\n"
        ),
        [
            "allow(clippy::all)",
            "allow(clippy::style)",
            "allow(clippy::disallowed_types)"
        ]
    );
}

// The Windows arm is cfg-disabled on Unix, so its capability boundary is checked from syntax on
// every host. The parser resolves the dependency's local aliases and tracks the one command value
// through the builder shapes allowed; the tests below are the gate's pass and fail
// controls, and production source goes through the same function.
mod windows_spawn_gate {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use syn::visit::{self, Visit};
    use syn::{Expr, ExprCall, ExprMethodCall, ItemFn, Pat, Stmt, Type, UseTree};

    const COMMAND_BUILDERS: &[&str] = &[
        "env_clear",
        "env",
        "envs",
        "env_remove",
        "current_dir",
        "stdin",
        "stdout",
        "stderr",
        "arg",
        "args",
        "raw_arg",
    ];
    const FORBIDDEN_COMMAND_METHODS: &[&str] = &[
        "spawn",
        "spawn_suspended",
        "spawn_suspended_with",
        "status",
        "status_with",
        "output",
        "output_with",
        "raw_arg",
        "arg_handle",
        "env_handle",
    ];
    const FORBIDDEN_TYPES: &[&str] = &[
        "Job",
        "ParentProcess",
        "SuspendedChild",
        "AsPseudoConsole",
        "MitigationPolicy",
    ];
    const WINDOWS_ITEMS: &[&str] = &[
        "Command",
        "Stdio",
        "SpawnOptions",
        "CreationFlags",
        "DropPolicy",
        "Job",
        "ParentProcess",
        "SuspendedChild",
        "AsPseudoConsole",
        "MitigationPolicy",
    ];

    #[derive(Clone, Default)]
    struct Bindings {
        names: BTreeMap<String, String>,
        windows_glob: bool,
    }

    impl Bindings {
        fn resolve(&self, path: &syn::Path) -> String {
            let segments: Vec<_> = path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            let Some(first) = segments.first() else {
                return String::new();
            };
            if let Some(bound) = self.names.get(first) {
                return std::iter::once(bound.as_str())
                    .chain(segments.iter().skip(1).map(String::as_str))
                    .collect::<Vec<_>>()
                    .join("::");
            }
            if self.windows_glob && WINDOWS_ITEMS.contains(&first.as_str()) {
                return std::iter::once("windows_spawn")
                    .chain(segments.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join("::");
            }
            segments.join("::")
        }
    }

    struct BindingVisitor<'a>(&'a mut Bindings);

    impl Visit<'_> for BindingVisitor<'_> {
        fn visit_item_use(&mut self, item: &syn::ItemUse) {
            bind_use(self.0, Vec::new(), &item.tree);
            visit::visit_item_use(self, item);
        }

        fn visit_item_type(&mut self, item: &syn::ItemType) {
            if let Type::Path(path) = item.ty.as_ref() {
                let resolved = self.0.resolve(&path.path);
                if resolved.starts_with("windows_spawn::") {
                    self.0.names.insert(item.ident.to_string(), resolved);
                }
            }
            visit::visit_item_type(self, item);
        }
    }

    fn bind_use(bindings: &mut Bindings, mut prefix: Vec<String>, tree: &UseTree) {
        match tree {
            UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                bind_use(bindings, prefix, &path.tree);
            }
            UseTree::Name(name) => {
                prefix.push(name.ident.to_string());
                let full = prefix.join("::");
                if full.starts_with("windows_spawn::") {
                    bindings.names.insert(name.ident.to_string(), full);
                }
            }
            UseTree::Rename(rename) => {
                prefix.push(rename.ident.to_string());
                let full = prefix.join("::");
                if full.starts_with("windows_spawn::") {
                    bindings.names.insert(rename.rename.to_string(), full);
                }
            }
            UseTree::Glob(_) => {
                if prefix == ["windows_spawn"] {
                    bindings.windows_glob = true;
                }
            }
            UseTree::Group(group) => {
                for item in &group.items {
                    bind_use(bindings, prefix.clone(), item);
                }
            }
        }
    }

    struct FunctionCollector<'ast> {
        functions: Vec<&'ast ItemFn>,
    }

    impl<'ast> Visit<'ast> for FunctionCollector<'ast> {
        fn visit_item_fn(&mut self, item: &'ast ItemFn) {
            self.functions.push(item);
            visit::visit_item_fn(self, item);
        }
    }

    struct Facts<'ast, 'bindings> {
        bindings: &'bindings Bindings,
        locals: BTreeMap<String, Vec<&'ast Expr>>,
        calls: Vec<&'ast ExprCall>,
        methods: Vec<&'ast ExprMethodCall>,
        paths: Vec<&'ast syn::Path>,
        command_news: BTreeSet<usize>,
        struct_values: Vec<&'ast Expr>,
        returned_values: Vec<&'ast Expr>,
        nonlocal_assignments: Vec<&'ast Expr>,
        macros: Vec<&'ast syn::Macro>,
    }

    impl<'ast, 'bindings> Facts<'ast, 'bindings> {
        fn new(bindings: &'bindings Bindings) -> Self {
            Self {
                bindings,
                locals: BTreeMap::new(),
                calls: Vec::new(),
                methods: Vec::new(),
                paths: Vec::new(),
                command_news: BTreeSet::new(),
                struct_values: Vec::new(),
                returned_values: Vec::new(),
                nonlocal_assignments: Vec::new(),
                macros: Vec::new(),
            }
        }

        fn values(&self, expression: &'ast Expr) -> BTreeSet<usize> {
            self.values_inner(expression, &mut BTreeSet::new())
        }

        fn values_inner(
            &self,
            expression: &'ast Expr,
            seen: &mut BTreeSet<String>,
        ) -> BTreeSet<usize> {
            match expression {
                Expr::Call(call)
                    if expr_path(&call.func).is_some_and(|path| {
                        self.bindings.resolve(path) == "windows_spawn::Command::new"
                    }) =>
                {
                    BTreeSet::from([std::ptr::from_ref(call) as usize])
                }
                Expr::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
                    let name = path.path.segments[0].ident.to_string();
                    if !seen.insert(name.clone()) {
                        return BTreeSet::new();
                    }
                    let mut values = BTreeSet::new();
                    if let Some(expressions) = self.locals.get(&name) {
                        for expression in expressions {
                            values.extend(self.values_inner(expression, seen));
                        }
                    }
                    seen.remove(&name);
                    values
                }
                Expr::Reference(reference) => self.values_inner(&reference.expr, seen),
                Expr::Paren(paren) => self.values_inner(&paren.expr, seen),
                Expr::Group(group) => self.values_inner(&group.expr, seen),
                Expr::Try(tried) => self.values_inner(&tried.expr, seen),
                Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Deref(_)) => {
                    self.values_inner(&unary.expr, seen)
                }
                Expr::Block(block) => block
                    .block
                    .stmts
                    .last()
                    .and_then(stmt_expr)
                    .map_or_else(BTreeSet::new, |tail| self.values_inner(tail, seen)),
                Expr::MethodCall(call)
                    if COMMAND_BUILDERS.contains(&call.method.to_string().as_str())
                        || matches!(
                            call.method.to_string().as_str(),
                            "unwrap" | "expect" | "map_err"
                        ) =>
                {
                    self.values_inner(&call.receiver, seen)
                }
                Expr::Call(call) => {
                    let Some(path) = expr_path(&call.func) else {
                        return BTreeSet::new();
                    };
                    let resolved = self.bindings.resolve(path);
                    let method = resolved.rsplit("::").next().unwrap_or_default();
                    if resolved.starts_with("windows_spawn::Command::")
                        && COMMAND_BUILDERS.contains(&method)
                    {
                        call.args
                            .first()
                            .map_or_else(BTreeSet::new, |first| self.values_inner(first, seen))
                    } else {
                        BTreeSet::new()
                    }
                }
                _ => BTreeSet::new(),
            }
        }
    }

    impl<'ast> Visit<'ast> for Facts<'ast, '_> {
        fn visit_local(&mut self, local: &'ast syn::Local) {
            if let Some(name) = pat_name(&local.pat) {
                let values = self.locals.entry(name).or_default();
                if let Some(init) = &local.init {
                    values.push(&init.expr);
                }
            }
            visit::visit_local(self, local);
        }

        fn visit_expr_assign(&mut self, assign: &'ast syn::ExprAssign) {
            if let Some(name) = path_name(&assign.left) {
                self.locals.entry(name).or_default().push(&assign.right);
            } else {
                self.nonlocal_assignments.push(&assign.right);
            }
            visit::visit_expr_assign(self, assign);
        }

        fn visit_expr_struct(&mut self, structure: &'ast syn::ExprStruct) {
            self.struct_values
                .extend(structure.fields.iter().map(|field| &field.expr));
            visit::visit_expr_struct(self, structure);
        }

        fn visit_expr_return(&mut self, returned: &'ast syn::ExprReturn) {
            if let Some(expression) = &returned.expr {
                self.returned_values.push(expression);
            }
            visit::visit_expr_return(self, returned);
        }

        fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
            self.macros.push(invocation);
            visit::visit_macro(self, invocation);
        }

        fn visit_expr_call(&mut self, call: &'ast ExprCall) {
            if expr_path(&call.func)
                .is_some_and(|path| self.bindings.resolve(path) == "windows_spawn::Command::new")
            {
                self.command_news.insert(std::ptr::from_ref(call) as usize);
            }
            self.calls.push(call);
            visit::visit_expr_call(self, call);
        }

        fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
            self.methods.push(call);
            visit::visit_expr_method_call(self, call);
        }

        fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
            self.paths.push(&path.path);
            visit::visit_expr_path(self, path);
        }

        fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
            self.paths.push(&path.path);
            visit::visit_type_path(self, path);
        }
    }

    fn pat_name(pattern: &Pat) -> Option<String> {
        match pattern {
            Pat::Ident(ident) => Some(ident.ident.to_string()),
            Pat::Type(typed) => pat_name(&typed.pat),
            _ => None,
        }
    }

    fn path_name(expression: &Expr) -> Option<String> {
        match expression {
            Expr::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
                Some(path.path.segments[0].ident.to_string())
            }
            Expr::Paren(paren) => path_name(&paren.expr),
            _ => None,
        }
    }

    fn expr_path(expression: &Expr) -> Option<&syn::Path> {
        match expression {
            Expr::Path(path) if path.qself.is_none() => Some(&path.path),
            _ => None,
        }
    }

    fn stmt_expr(statement: &Stmt) -> Option<&Expr> {
        match statement {
            Stmt::Expr(expression, _) => Some(expression),
            _ => None,
        }
    }

    enum SpawnSite<'ast> {
        Method(&'ast ExprMethodCall),
        Ufcs(&'ast ExprCall),
    }

    impl<'ast> SpawnSite<'ast> {
        fn receiver(&self) -> Option<&'ast Expr> {
            match self {
                Self::Method(call) => Some(&call.receiver),
                Self::Ufcs(call) => call.args.first(),
            }
        }

        fn options(&self) -> Option<&'ast Expr> {
            match self {
                Self::Method(call) => call.args.first(),
                Self::Ufcs(call) => call.args.iter().nth(1),
            }
        }
    }

    struct Association<'ast> {
        slot: &'static str,
        receiver: &'ast Expr,
        argument: Option<&'ast Expr>,
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    enum StdioKind {
        Null,
        Inherit,
        Piped,
        Handle(Option<String>),
        Unknown,
    }

    impl StdioKind {
        fn label(&self) -> &'static str {
            match self {
                Self::Null => "Null",
                Self::Inherit => "Inherit",
                Self::Piped => "Piped",
                Self::Handle(_) => "Handle",
                Self::Unknown => "Unknown",
            }
        }
    }

    #[derive(Default)]
    struct SpawnOptionsShape<'ast> {
        new_count: usize,
        creation_flags: Vec<&'ast Expr>,
        drop_policies: Vec<&'ast Expr>,
        opaque: Option<String>,
    }

    fn check_texts(texts: &[&str]) -> Vec<String> {
        let mut violations = BTreeSet::new();
        let mut spawn_count = 0usize;
        for text in texts {
            let file = syn::parse_file(text).expect("the source parses as Rust");
            let mut file_bindings = Bindings::default();
            BindingVisitor(&mut file_bindings).visit_file(&file);
            let mut collector = FunctionCollector {
                functions: Vec::new(),
            };
            collector.visit_file(&file);
            for function in collector.functions {
                let mut bindings = file_bindings.clone();
                BindingVisitor(&mut bindings).visit_item_fn(function);
                let (count, found) = check_function(function, &bindings);
                spawn_count += count;
                violations.extend(found);
            }
        }
        if spawn_count != 1 {
            violations.insert(format!("spawn-count: {spawn_count}"));
        }
        violations.into_iter().collect()
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one audit pass reports every receiver and capability violation together"
    )]
    fn check_function(function: &ItemFn, bindings: &Bindings) -> (usize, BTreeSet<String>) {
        let mut facts = Facts::new(bindings);
        facts.visit_item_fn(function);
        let mut spawns = Vec::new();
        for call in &facts.methods {
            if call.method == "spawn_with" {
                spawns.push(SpawnSite::Method(call));
            }
        }
        for call in &facts.calls {
            if expr_path(&call.func)
                .is_some_and(|path| bindings.resolve(path) == "windows_spawn::Command::spawn_with")
            {
                spawns.push(SpawnSite::Ufcs(call));
            }
        }
        let mut violations = BTreeSet::new();
        if spawns.is_empty() {
            if !facts.command_news.is_empty()
                || facts.calls.iter().any(|call| {
                    expr_path(&call.func)
                        .is_some_and(|path| bindings.resolve(path).starts_with("windows_spawn::"))
                })
            {
                check_capabilities(&facts, bindings, None, None, &mut violations);
            }
            return (0, violations);
        }

        if facts.command_news.len() != 1 {
            violations.insert(format!("command-count: {}", facts.command_news.len()));
        }
        let spawned_values = spawns
            .first()
            .and_then(SpawnSite::receiver)
            .map_or_else(BTreeSet::new, |receiver| facts.values(receiver));
        let spawned = spawned_values.iter().next().copied();
        if spawned_values.len() != 1 {
            violations.insert(format!(
                "spawn-receiver: {}",
                spawns
                    .first()
                    .and_then(SpawnSite::receiver)
                    .map_or_else(|| "missing".to_owned(), expr_label)
            ));
        }

        for (name, expressions) in &facts.locals {
            if expressions.len() > 1 {
                let values: BTreeSet<_> = expressions
                    .iter()
                    .flat_map(|expression| facts.values(expression))
                    .collect();
                if values.len() > 1 {
                    violations.insert(format!("ambiguous-receiver: {name}"));
                }
            }
        }

        let associations = associations(&facts, bindings);
        let mut by_slot: BTreeMap<&str, Vec<&Association<'_>>> = BTreeMap::new();
        for association in &associations {
            if let Some(argument) = association.argument {
                violations.extend(unresolved_paths(argument, &facts, bindings));
            }
            let values = facts.values(association.receiver);
            if spawned.is_some_and(|spawned| values == BTreeSet::from([spawned])) {
                by_slot
                    .entry(association.slot)
                    .or_default()
                    .push(association);
            } else if !values.is_empty() {
                violations.insert(format!(
                    "stray-association: {} on {}",
                    association.slot,
                    expr_label(association.receiver)
                ));
            }
        }
        for slot in ["stdin", "stdout", "stderr"] {
            let count = by_slot.get(slot).map_or(0, Vec::len);
            if count != 1 {
                violations.insert(format!("{slot}-count: {count}"));
            }
        }

        let log = facts.locals.iter().find_map(|(name, expressions)| {
            (expressions.len() == 1 && is_log_open(expressions[0], &facts)).then(|| name.clone())
        });
        let clones: BTreeSet<_> = log.as_ref().map_or_else(BTreeSet::new, |log| {
            facts
                .locals
                .iter()
                .filter(|(_, expressions)| {
                    expressions.len() == 1 && is_log_clone(expressions[0], log, &facts)
                })
                .map(|(name, _)| name.clone())
                .collect()
        });
        if clones.len() != 2 {
            violations.insert(format!("log-clone-count: {}", clones.len()));
        }

        let mut consumed = BTreeSet::new();
        if let Some(stdin) = one_association(&by_slot, "stdin") {
            let kind = stdin.argument.map_or(StdioKind::Unknown, |argument| {
                stdio(argument, &facts, &clones)
            });
            if kind != StdioKind::Null {
                violations.insert(format!("stdin-argument: {}", kind.label()));
            }
        }
        for slot in ["stdout", "stderr"] {
            if let Some(association) = one_association(&by_slot, slot) {
                let kind = association.argument.map_or(StdioKind::Unknown, |argument| {
                    stdio(argument, &facts, &clones)
                });
                match kind {
                    StdioKind::Handle(Some(name)) if clones.contains(&name) => {
                        if !consumed.insert(name) {
                            violations.insert(format!("{slot}-argument: Reused"));
                        }
                    }
                    _ => {
                        violations.insert(format!("{slot}-argument: {}", kind.label()));
                    }
                }
            }
        }
        for clone in clones.difference(&consumed) {
            violations.insert(format!("log-clone-unused: {clone}"));
        }

        match spawns.first().and_then(SpawnSite::options) {
            Some(options) => check_spawn_options(options, &facts, bindings, &mut violations),
            None => {
                violations.insert("spawn-options: missing".to_owned());
            }
        }
        check_capabilities(&facts, bindings, spawned, spawns.first(), &mut violations);
        if let Some(spawn) = spawns.first() {
            if let Some(receiver) = spawn.receiver() {
                violations.extend(unresolved_paths(receiver, &facts, bindings));
            }
            if let Some(options) = spawn.options() {
                violations.extend(unresolved_paths(options, &facts, bindings));
            }
        }
        for invocation in &facts.macros {
            violations.insert(format!(
                "macro-in-spawn-item: {}",
                path_label(&invocation.path)
            ));
        }
        if let Some(spawned) = spawned {
            for expression in facts
                .struct_values
                .iter()
                .chain(&facts.returned_values)
                .chain(&facts.nonlocal_assignments)
            {
                if facts.values(expression).contains(&spawned) {
                    violations.insert(format!("command-escape: {}", expr_label(expression)));
                }
            }
            if let Some(tail) = function.block.stmts.last().and_then(stmt_expr)
                && facts.values(tail).contains(&spawned)
            {
                violations.insert(format!("command-escape: {}", expr_label(tail)));
            }
        }
        (spawns.len(), violations)
    }

    fn check_spawn_options(
        expression: &Expr,
        facts: &Facts<'_, '_>,
        bindings: &Bindings,
        violations: &mut BTreeSet<String>,
    ) {
        let mut shape = SpawnOptionsShape::default();
        inspect_spawn_options(
            expression,
            facts,
            bindings,
            &mut BTreeSet::new(),
            &mut shape,
        );
        if let Some(opaque) = shape.opaque {
            violations.insert(format!("spawn-options: {opaque}"));
            return;
        }
        if shape.new_count != 1 {
            violations.insert(format!("spawn-options-new-count: {}", shape.new_count));
        }
        if shape.creation_flags.len() == 1 {
            let flags = creation_flags(shape.creation_flags[0], bindings);
            let expected = ["DETACHED_PROCESS", "NEW_PROCESS_GROUP"];
            if flags != expected {
                violations.insert(format!("spawn-options-creation-flags: {flags:?}"));
            }
        } else {
            violations.insert(format!(
                "spawn-options-creation-flags-count: {}",
                shape.creation_flags.len()
            ));
        }
        if shape.drop_policies.len() != 1 {
            violations.insert(format!(
                "spawn-options-drop-policy-count: {}",
                shape.drop_policies.len()
            ));
        } else if option_path(shape.drop_policies[0], bindings).as_deref()
            != Some("windows_spawn::DropPolicy::Detach")
        {
            violations.insert(format!(
                "spawn-options-drop-policy: {}",
                option_path(shape.drop_policies[0], bindings)
                    .unwrap_or_else(|| "opaque expression".to_owned())
            ));
        }
    }

    fn inspect_spawn_options<'ast>(
        expression: &'ast Expr,
        facts: &Facts<'ast, '_>,
        bindings: &Bindings,
        seen: &mut BTreeSet<String>,
        shape: &mut SpawnOptionsShape<'ast>,
    ) {
        match expression {
            Expr::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
                let name = path.path.segments[0].ident.to_string();
                if !seen.insert(name.clone()) {
                    shape.opaque = Some(format!("cyclic local {name}"));
                    return;
                }
                match facts.locals.get(&name).map(Vec::as_slice) {
                    Some([value]) => inspect_spawn_options(value, facts, bindings, seen, shape),
                    Some(values) => {
                        shape.opaque = Some(format!(
                            "ambiguous local {name} with {} values",
                            values.len()
                        ));
                    }
                    None => shape.opaque = Some(format!("unresolved local {name}")),
                }
                seen.remove(&name);
            }
            Expr::Call(call)
                if expr_path(&call.func).is_some_and(|path| {
                    bindings.resolve(path) == "windows_spawn::SpawnOptions::new"
                }) =>
            {
                shape.new_count += 1;
            }
            Expr::MethodCall(call) if call.method == "creation_flags" => {
                inspect_spawn_options(&call.receiver, facts, bindings, seen, shape);
                if let Some(flags) = call.args.first() {
                    shape.creation_flags.push(flags);
                }
            }
            Expr::MethodCall(call) if call.method == "drop_policy" => {
                inspect_spawn_options(&call.receiver, facts, bindings, seen, shape);
                if let Some(policy) = call.args.first() {
                    shape.drop_policies.push(policy);
                }
            }
            Expr::Call(call) => {
                let resolved = expr_path(&call.func).map(|path| bindings.resolve(path));
                match resolved.as_deref() {
                    Some("windows_spawn::SpawnOptions::creation_flags") => {
                        if let Some(options) = call.args.first() {
                            inspect_spawn_options(options, facts, bindings, seen, shape);
                        }
                        if let Some(flags) = call.args.iter().nth(1) {
                            shape.creation_flags.push(flags);
                        }
                    }
                    Some("windows_spawn::SpawnOptions::drop_policy") => {
                        if let Some(options) = call.args.first() {
                            inspect_spawn_options(options, facts, bindings, seen, shape);
                        }
                        if let Some(policy) = call.args.iter().nth(1) {
                            shape.drop_policies.push(policy);
                        }
                    }
                    _ => shape.opaque = Some("opaque expression".to_owned()),
                }
            }
            Expr::Reference(reference) => {
                inspect_spawn_options(&reference.expr, facts, bindings, seen, shape);
            }
            Expr::Paren(paren) => {
                inspect_spawn_options(&paren.expr, facts, bindings, seen, shape);
            }
            Expr::Group(group) => {
                inspect_spawn_options(&group.expr, facts, bindings, seen, shape);
            }
            Expr::Try(tried) => {
                inspect_spawn_options(&tried.expr, facts, bindings, seen, shape);
            }
            Expr::Block(block) => match block.block.stmts.last().and_then(stmt_expr) {
                Some(tail) => inspect_spawn_options(tail, facts, bindings, seen, shape),
                None => shape.opaque = Some("opaque block".to_owned()),
            },
            Expr::MethodCall(call)
                if matches!(
                    call.method.to_string().as_str(),
                    "unwrap" | "expect" | "map_err"
                ) =>
            {
                inspect_spawn_options(&call.receiver, facts, bindings, seen, shape);
            }
            Expr::Macro(expression) => {
                shape.opaque = Some(format!("opaque macro {}", path_label(&expression.mac.path)));
            }
            _ => shape.opaque = Some("opaque expression".to_owned()),
        }
    }

    fn creation_flags(expression: &Expr, bindings: &Bindings) -> Vec<&'static str> {
        let mut flags = Vec::new();
        collect_creation_flags(expression, bindings, &mut flags);
        flags.sort_unstable();
        flags
    }

    fn collect_creation_flags(
        expression: &Expr,
        bindings: &Bindings,
        flags: &mut Vec<&'static str>,
    ) {
        match expression {
            Expr::Binary(binary) if matches!(binary.op, syn::BinOp::BitOr(_)) => {
                collect_creation_flags(&binary.left, bindings, flags);
                collect_creation_flags(&binary.right, bindings, flags);
            }
            Expr::Paren(paren) => collect_creation_flags(&paren.expr, bindings, flags),
            Expr::Group(group) => collect_creation_flags(&group.expr, bindings, flags),
            Expr::Path(path) => match bindings.resolve(&path.path).as_str() {
                "windows_spawn::CreationFlags::DETACHED_PROCESS" => {
                    flags.push("DETACHED_PROCESS");
                }
                "windows_spawn::CreationFlags::NEW_PROCESS_GROUP" => {
                    flags.push("NEW_PROCESS_GROUP");
                }
                _ => flags.push("OTHER"),
            },
            _ => flags.push("OPAQUE"),
        }
    }

    fn option_path(expression: &Expr, bindings: &Bindings) -> Option<String> {
        match expression {
            Expr::Path(path) => Some(bindings.resolve(&path.path)),
            Expr::Paren(paren) => option_path(&paren.expr, bindings),
            Expr::Group(group) => option_path(&group.expr, bindings),
            _ => None,
        }
    }

    fn path_label(path: &syn::Path) -> String {
        path.segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::")
    }

    fn unresolved_paths(
        expression: &Expr,
        facts: &Facts<'_, '_>,
        bindings: &Bindings,
    ) -> BTreeSet<String> {
        struct Paths<'ast>(Vec<&'ast syn::Path>);

        impl<'ast> Visit<'ast> for Paths<'ast> {
            fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
                self.0.push(&path.path);
                visit::visit_expr_path(self, path);
            }
        }

        let mut paths = Paths(Vec::new());
        paths.visit_expr(expression);
        paths
            .0
            .into_iter()
            .filter_map(|path| {
                let first = path.segments.first()?.ident.to_string();
                let known = matches!(
                    first.as_str(),
                    "windows_spawn" | "std" | "crate" | "self" | "super" | "From" | "Into"
                ) || bindings.names.contains_key(&first)
                    || facts.locals.contains_key(&first)
                    || (bindings.windows_glob && WINDOWS_ITEMS.contains(&first.as_str()));
                (!known).then(|| format!("unresolved-path: {}", bindings.resolve(path)))
            })
            .collect()
    }

    fn associations<'ast>(facts: &Facts<'ast, '_>, bindings: &Bindings) -> Vec<Association<'ast>> {
        let mut found = Vec::new();
        for call in &facts.methods {
            if let Some(slot) = slot(&call.method.to_string()) {
                let values = facts.values(&call.receiver);
                if !values.is_empty() {
                    found.push(Association {
                        slot,
                        receiver: &call.receiver,
                        argument: call.args.first(),
                    });
                }
            }
        }
        for call in &facts.calls {
            let Some(path) = expr_path(&call.func) else {
                continue;
            };
            let resolved = bindings.resolve(path);
            if !resolved.starts_with("windows_spawn::Command::") {
                continue;
            }
            let Some(slot) = resolved.rsplit("::").next().and_then(slot) else {
                continue;
            };
            if let Some(receiver) = call.args.first() {
                found.push(Association {
                    slot,
                    receiver,
                    argument: call.args.iter().nth(1),
                });
            }
        }
        found
    }

    fn slot(name: &str) -> Option<&'static str> {
        match name {
            "stdin" => Some("stdin"),
            "stdout" => Some("stdout"),
            "stderr" => Some("stderr"),
            _ => None,
        }
    }

    fn one_association<'a>(
        by_slot: &'a BTreeMap<&str, Vec<&'a Association<'a>>>,
        slot: &str,
    ) -> Option<&'a Association<'a>> {
        let found = by_slot.get(slot)?;
        (found.len() == 1).then_some(found[0])
    }

    fn is_log_open(expression: &Expr, facts: &Facts<'_, '_>) -> bool {
        let expression = unwrap(expression);
        let Expr::MethodCall(open) = expression else {
            return false;
        };
        if open.method != "open" {
            return false;
        }
        open.args.first().is_some_and(|path| {
            let Some(name) = file_name(path, facts, &mut BTreeSet::new()) else {
                return false;
            };
            facts
                .locals
                .get(&name)
                .is_some_and(|values| values.len() == 1 && is_log_path(values[0]))
        })
    }

    fn is_log_path(expression: &Expr) -> bool {
        let expression = unwrap(expression);
        matches!(
            expression,
            Expr::MethodCall(call)
                if call.method == "join"
                    && call.args.first().and_then(path_name).as_deref() == Some("LOG_NAME")
        )
    }

    fn is_log_clone(expression: &Expr, log: &str, facts: &Facts<'_, '_>) -> bool {
        let expression = unwrap(expression);
        matches!(
            expression,
            Expr::MethodCall(call)
                if call.method == "try_clone"
                    && file_name(&call.receiver, facts, &mut BTreeSet::new()).as_deref()
                        == Some(log)
        )
    }

    fn unwrap(mut expression: &Expr) -> &Expr {
        loop {
            expression = match expression {
                Expr::Reference(reference) => &reference.expr,
                Expr::Paren(paren) => &paren.expr,
                Expr::Group(group) => &group.expr,
                Expr::Try(tried) => &tried.expr,
                Expr::MethodCall(call)
                    if matches!(
                        call.method.to_string().as_str(),
                        "unwrap" | "expect" | "map_err"
                    ) =>
                {
                    &call.receiver
                }
                _ => return expression,
            };
        }
    }

    fn file_name(
        expression: &Expr,
        facts: &Facts<'_, '_>,
        seen: &mut BTreeSet<String>,
    ) -> Option<String> {
        let expression = unwrap(expression);
        let name = path_name(expression)?;
        if !seen.insert(name.clone()) {
            return None;
        }
        if let Some(values) = facts.locals.get(&name)
            && values.len() == 1
            && let Some(inner) = path_name(unwrap(values[0]))
        {
            return Some(inner);
        }
        Some(name)
    }

    fn stdio(expression: &Expr, facts: &Facts<'_, '_>, clones: &BTreeSet<String>) -> StdioKind {
        stdio_inner(expression, facts, clones, &mut BTreeSet::new())
    }

    fn stdio_inner(
        expression: &Expr,
        facts: &Facts<'_, '_>,
        clones: &BTreeSet<String>,
        seen: &mut BTreeSet<String>,
    ) -> StdioKind {
        let expression = unwrap(expression);
        if let Some(name) = path_name(expression) {
            if clones.contains(&name) {
                return StdioKind::Handle(Some(name));
            }
            if !seen.insert(name.clone()) {
                return StdioKind::Unknown;
            }
            return facts
                .locals
                .get(&name)
                .and_then(|values| {
                    (values.len() == 1).then(|| stdio_inner(values[0], facts, clones, seen))
                })
                .unwrap_or(StdioKind::Unknown);
        }
        match expression {
            Expr::Call(call) => {
                let Some(path) = expr_path(&call.func) else {
                    return StdioKind::Unknown;
                };
                let resolved = facts.bindings.resolve(path);
                match resolved.as_str() {
                    "windows_spawn::Stdio::null" => StdioKind::Null,
                    "windows_spawn::Stdio::inherit" => StdioKind::Inherit,
                    "windows_spawn::Stdio::piped" => StdioKind::Piped,
                    "windows_spawn::Stdio::from"
                    | "windows_spawn::Stdio::from_borrowed"
                    | "From::from"
                    | "Into::into" => StdioKind::Handle(
                        call.args
                            .first()
                            .and_then(|argument| file_name(argument, facts, &mut BTreeSet::new())),
                    ),
                    _ => StdioKind::Unknown,
                }
            }
            Expr::MethodCall(call) if call.method == "into" => {
                StdioKind::Handle(file_name(&call.receiver, facts, &mut BTreeSet::new()))
            }
            _ => StdioKind::Unknown,
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one audit pass reports every forbidden windows-spawn capability together"
    )]
    fn check_capabilities(
        facts: &Facts<'_, '_>,
        bindings: &Bindings,
        spawned: Option<usize>,
        spawn: Option<&SpawnSite<'_>>,
        violations: &mut BTreeSet<String>,
    ) {
        for path in &facts.paths {
            let resolved = bindings.resolve(path);
            if FORBIDDEN_TYPES
                .iter()
                .any(|name| resolved == format!("windows_spawn::{name}"))
            {
                violations.insert(format!("forbidden-type: {resolved}"));
            }
        }
        let detach = facts
            .paths
            .iter()
            .filter(|path| bindings.resolve(path) == "windows_spawn::DropPolicy::Detach")
            .count();
        if detach != 1 {
            violations.insert(format!("drop-policy-detach-count: {detach}"));
        }
        if facts
            .paths
            .iter()
            .any(|path| bindings.resolve(path) == "windows_spawn::DropPolicy::KillTree")
        {
            violations.insert("forbidden-drop-policy: KillTree".to_owned());
        }
        let flags: Vec<_> = facts
            .paths
            .iter()
            .filter_map(|path| {
                bindings
                    .resolve(path)
                    .strip_prefix("windows_spawn::CreationFlags::")
                    .map(str::to_owned)
            })
            .collect();
        for expected in ["DETACHED_PROCESS", "NEW_PROCESS_GROUP"] {
            let count = flags
                .iter()
                .filter(|flag| flag.as_str() == expected)
                .count();
            if count != 1 {
                violations.insert(format!("creation-flag-{expected}-count: {count}"));
            }
        }
        for flag in &flags {
            if !matches!(flag.as_str(), "DETACHED_PROCESS" | "NEW_PROCESS_GROUP") {
                violations.insert(format!("forbidden-creation-flag: {flag}"));
            }
        }
        for call in &facts.methods {
            let method = call.method.to_string();
            let values = facts.values(&call.receiver);
            let is_command =
                !values.is_empty() && spawned.is_none_or(|spawned| values.contains(&spawned));
            if is_command {
                if FORBIDDEN_COMMAND_METHODS.contains(&method.as_str()) {
                    violations.insert(format!("forbidden-command-method: {method}"));
                } else if matches!(method.as_str(), "arg" | "args") {
                    violations.insert("argv-nonempty".to_owned());
                } else if !COMMAND_BUILDERS.contains(&method.as_str()) && method != "spawn_with" {
                    violations.insert(format!("command-escape: {}", expr_label(&call.receiver)));
                }
            }
            if matches!(
                method.as_str(),
                "job" | "parent_process" | "pseudoconsole" | "mitigation"
            ) {
                violations.insert(format!("forbidden-spawn-option: {method}"));
            }
        }
        for call in &facts.calls {
            let Some(path) = expr_path(&call.func) else {
                continue;
            };
            let resolved = bindings.resolve(path);
            if let Some(method) = resolved.strip_prefix("windows_spawn::Command::") {
                if FORBIDDEN_COMMAND_METHODS.contains(&method) {
                    violations.insert(format!("forbidden-command-method: {method}"));
                } else if matches!(method, "arg" | "args") {
                    violations.insert("argv-nonempty".to_owned());
                }
            }
        }
        if let Some(options) = spawn.and_then(SpawnSite::options)
            && facts.values(options).contains(&spawned.unwrap_or_default())
        {
            violations.insert("command-escape: spawn options".to_owned());
        }
        for call in &facts.calls {
            let recognized = expr_path(&call.func).is_some_and(|path| {
                let resolved = bindings.resolve(path);
                resolved == "windows_spawn::Command::new"
                    || resolved.starts_with("windows_spawn::Command::")
            });
            if !recognized
                && call.args.iter().any(|argument| {
                    spawned.is_some_and(|spawned| facts.values(argument).contains(&spawned))
                })
            {
                violations.insert(format!("command-escape: {}", expr_label(&call.func)));
            }
        }
    }

    fn expr_label(expression: &Expr) -> String {
        path_name(expression).unwrap_or_else(|| "expression".to_owned())
    }

    fn production_violations() -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut texts = Vec::new();
        super::walk(&root, &mut |path| {
            if path.extension().is_some_and(|extension| extension == "rs") {
                texts.push(
                    std::fs::read_to_string(path)
                        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display())),
                );
            }
        });
        check_texts(&texts.iter().map(String::as_str).collect::<Vec<_>>())
    }

    const FIXTURE: &str = r"
fn spawn() -> Result<(), Error> {
    let log_path = runtime.join(LOG_NAME);
    let mut open = OpenOptions::new();
    let mut log = open.open(&log_path).map_err(error)?;
    let stdout = log.try_clone().map_err(error)?;
    let stderr = log.try_clone().map_err(error)?;
    let mut command = windows_spawn::Command::new(&daemon);
    command.env_clear();
    command.stdin(windows_spawn::Stdio::null());
    command.stdout(windows_spawn::Stdio::from(stdout));
    command.stderr(windows_spawn::Stdio::from(stderr));
    let options = windows_spawn::SpawnOptions::new()
        .creation_flags(windows_spawn::CreationFlags::DETACHED_PROCESS
            | windows_spawn::CreationFlags::NEW_PROCESS_GROUP)
        .drop_policy(windows_spawn::DropPolicy::Detach);
    command.spawn_with(options)?;
    Ok(())
}
";

    fn mutant(from: &str, to: &str) -> String {
        assert_eq!(
            FIXTURE.matches(from).count(),
            1,
            "the mutation target drifted"
        );
        FIXTURE.replacen(from, to, 1)
    }

    fn check(text: &str) -> Vec<String> {
        check_texts(&[text])
    }

    #[test]
    fn the_production_windows_spawn_has_the_exact_handle_boundary() {
        assert_eq!(production_violations(), Vec::<String>::new());
    }

    #[test]
    fn association_mutants_print_their_exact_red_sets() {
        let arms = [
            (
                "mutant 1",
                mutant("    command.stdin(windows_spawn::Stdio::null());\n", ""),
                vec!["stdin-count: 0"],
            ),
            (
                "mutant 2",
                mutant(
                    "windows_spawn::Stdio::null()",
                    "windows_spawn::Stdio::inherit()",
                ),
                vec!["stdin-argument: Inherit"],
            ),
            (
                "mutant 3",
                mutant(
                    "windows_spawn::Stdio::null()",
                    "windows_spawn::Stdio::piped()",
                ),
                vec!["stdin-argument: Piped"],
            ),
            (
                "M4",
                mutant(
                    "    command.stdout(windows_spawn::Stdio::from(stdout));\n",
                    "",
                ),
                vec!["log-clone-unused: stdout", "stdout-count: 0"],
            ),
            (
                "M5",
                mutant(
                    "    command.stderr(windows_spawn::Stdio::from(stderr));\n",
                    "",
                ),
                vec!["log-clone-unused: stderr", "stderr-count: 0"],
            ),
            (
                "M6",
                mutant(
                    "windows_spawn::Stdio::from(stdout)",
                    "windows_spawn::Stdio::null()",
                ),
                vec!["log-clone-unused: stdout", "stdout-argument: Null"],
            ),
            (
                "M7",
                mutant(
                    "windows_spawn::Stdio::from(stderr)",
                    "windows_spawn::Stdio::null()",
                ),
                vec!["log-clone-unused: stderr", "stderr-argument: Null"],
            ),
            (
                "M8",
                mutant(
                    "    command.stdin(windows_spawn::Stdio::null());\n",
                    "    let mut decoy = windows_spawn::Command::new(&daemon);\n    decoy.stdin(windows_spawn::Stdio::null());\n",
                ),
                vec![
                    "command-count: 2",
                    "stdin-count: 0",
                    "stray-association: stdin on decoy",
                ],
            ),
            (
                "M9",
                mutant(
                    "    command.stdout(windows_spawn::Stdio::from(stdout));\n",
                    "    let mut decoy = windows_spawn::Command::new(&daemon);\n    decoy.stdout(windows_spawn::Stdio::from(stdout));\n",
                ),
                vec![
                    "command-count: 2",
                    "log-clone-unused: stdout",
                    "stdout-count: 0",
                    "stray-association: stdout on decoy",
                ],
            ),
        ];
        for (name, source, expected) in arms {
            assert_eq!(
                check(&source),
                expected,
                "{name} did not produce its exact red set"
            );
        }
    }

    #[test]
    fn command_tracking_accepts_chain_alias_move_and_ufcs_forms() {
        let chain = FIXTURE.replace(
            "    command.env_clear();\n    command.stdin(windows_spawn::Stdio::null());\n    command.stdout(windows_spawn::Stdio::from(stdout));\n    command.stderr(windows_spawn::Stdio::from(stderr));\n",
            "",
        ).replace(
            "    command.spawn_with(options)?;",
            "    command.env_clear().stdin(windows_spawn::Stdio::null()).stdout(windows_spawn::Stdio::from(stdout)).stderr(windows_spawn::Stdio::from(stderr)).spawn_with(options)?;",
        );
        let alias = FIXTURE
            .replace(
                "    command.env_clear();",
                "    let handle = &mut command;\n    handle.env_clear();",
            )
            .replace("    command.stdin", "    handle.stdin")
            .replace("    command.stdout", "    handle.stdout")
            .replace("    command.stderr", "    handle.stderr")
            .replace("    command.spawn_with", "    handle.spawn_with");
        let moved = FIXTURE
            .replace(
                "    command.env_clear();",
                "    let mut spawned = command;\n    spawned.env_clear();",
            )
            .replace("    command.stdin", "    spawned.stdin")
            .replace("    command.stdout", "    spawned.stdout")
            .replace("    command.stderr", "    spawned.stderr")
            .replace("    command.spawn_with", "    spawned.spawn_with");
        let ufcs = FIXTURE
            .replace("    command.stdin(windows_spawn::Stdio::null());", "    windows_spawn::Command::stdin(&mut command, windows_spawn::Stdio::null());")
            .replace("    command.stdout(windows_spawn::Stdio::from(stdout));", "    windows_spawn::Command::stdout(&mut command, windows_spawn::Stdio::from(stdout));")
            .replace("    command.stderr(windows_spawn::Stdio::from(stderr));", "    windows_spawn::Command::stderr(&mut command, windows_spawn::Stdio::from(stderr));");
        for (name, source) in [
            ("chain", chain),
            ("alias", alias),
            ("move", moved),
            ("UFCS", ufcs),
        ] {
            assert_eq!(
                check(&source),
                Vec::<String>::new(),
                "{name} tracking failed"
            );
        }
    }

    #[test]
    fn aliases_and_forbidden_capabilities_are_resolved_semantically() {
        let aliased = FIXTURE
            .replace(
                "fn spawn()",
                "use windows_spawn::{Command as Runner, DropPolicy as Policy};\nfn spawn()",
            )
            .replace("windows_spawn::Command::new", "Runner::new")
            .replace("windows_spawn::DropPolicy::Detach", "Policy::KillTree");
        let red = check(&aliased);
        assert!(
            red.contains(&"drop-policy-detach-count: 0".to_owned()),
            "{red:?}"
        );
        assert!(
            red.contains(&"forbidden-drop-policy: KillTree".to_owned()),
            "{red:?}"
        );

        let terminal = FIXTURE.replace(
            "command.spawn_with(options)?",
            "windows_spawn::Command::output_with(\n        &mut command, options)?",
        );
        let red = check(&terminal);
        assert!(
            red.contains(&"forbidden-command-method: output_with".to_owned()),
            "{red:?}"
        );
    }

    #[test]
    fn single_brace_glob_and_type_aliases_resolve_to_the_same_command() {
        let single = FIXTURE
            .replace(
                "fn spawn()",
                "use windows_spawn::Command as Runner;\nfn spawn()",
            )
            .replace("windows_spawn::Command::new", "Runner::new");
        let brace = FIXTURE
            .replace(
                "fn spawn()",
                "use windows_spawn::{Command as Runner};\nfn spawn()",
            )
            .replace("windows_spawn::Command::new", "Runner::new");
        let glob = FIXTURE
            .replace("windows_spawn::", "")
            .replace("fn spawn()", "use windows_spawn::*;\nfn spawn()");
        let type_alias = FIXTURE
            .replace(
                "fn spawn()",
                "type Runner = windows_spawn::Command;\nfn spawn()",
            )
            .replace("windows_spawn::Command::new", "Runner::new");
        for (name, source) in [
            ("single", single),
            ("brace", brace),
            ("glob", glob),
            ("type alias", type_alias),
        ] {
            assert_eq!(
                check(&source),
                Vec::<String>::new(),
                "{name} did not resolve"
            );
        }
    }

    #[test]
    fn every_terminal_method_and_unused_capability_has_a_fail_side_control() {
        let terminal_arms = [
            ("spawn", "command.spawn()"),
            ("spawn_suspended", "command.spawn_suspended()"),
            (
                "spawn_suspended_with",
                "command.spawn_suspended_with(options)",
            ),
            ("status", "command.status()"),
            ("status_with", "command.status_with(options)"),
            ("output", "command.output()"),
            ("output_with", "command.output_with(options)"),
            ("raw_arg", "command.raw_arg(\"x\")"),
            ("arg_handle", "command.arg_handle(&handle)"),
            ("env_handle", "command.env_handle(\"X\", &handle)"),
        ];
        for (method, replacement) in terminal_arms {
            let source = FIXTURE.replace("command.spawn_with(options)", replacement);
            let red = check(&source);
            assert!(
                red.contains(&format!("forbidden-command-method: {method}")),
                "{method} was not rejected: {red:?}"
            );
        }

        for capability in FORBIDDEN_TYPES {
            let source = FIXTURE.replace(
                "    let mut command = windows_spawn::Command::new(&daemon);",
                &format!(
                    "    let _forbidden: Option<windows_spawn::{capability}> = None;\n    let mut command = windows_spawn::Command::new(&daemon);"
                ),
            );
            let red = check(&source);
            assert!(
                red.contains(&format!("forbidden-type: windows_spawn::{capability}")),
                "{capability} was not rejected: {red:?}"
            );
        }

        for method in ["job", "parent_process", "pseudoconsole", "mitigation"] {
            let source = FIXTURE.replace(
                "windows_spawn::SpawnOptions::new()",
                &format!("windows_spawn::SpawnOptions::new().{method}(&capability)"),
            );
            let red = check(&source);
            assert!(
                red.contains(&format!("forbidden-spawn-option: {method}")),
                "SpawnOptions::{method} was not rejected: {red:?}"
            );
        }

        let renamed_flag = FIXTURE
            .replace(
                "fn spawn()",
                "use windows_spawn::CreationFlags as Flags;\nfn spawn()",
            )
            .replace(
                "windows_spawn::CreationFlags::NEW_PROCESS_GROUP",
                "Flags::NO_WINDOW",
            );
        let red = check(&renamed_flag);
        assert!(
            red.contains(&"forbidden-creation-flag: NO_WINDOW".to_owned()),
            "{red:?}"
        );

        let unresolved = FIXTURE.replace("windows_spawn::Stdio::null()", "mystery::Stdio::null()");
        let red = check(&unresolved);
        assert!(
            red.contains(&"unresolved-path: mystery::Stdio::null".to_owned()),
            "{red:?}"
        );

        let structure = FIXTURE.replace(
            "    command.env_clear();",
            "    let _wrapped = Wrapper { command };\n    command.env_clear();",
        );
        let red = check(&structure);
        assert!(
            red.contains(&"command-escape: command".to_owned()),
            "{red:?}"
        );

        let returned = FIXTURE.replace("    Ok(())", "    return command;");
        let red = check(&returned);
        assert!(
            red.contains(&"command-escape: command".to_owned()),
            "{red:?}"
        );
    }

    #[test]
    fn actual_spawn_options_and_command_macro_escapes_are_not_opaque() {
        let dead_options = FIXTURE
            .replace(
                "fn spawn()",
                "macro_rules! kill_tree_options {\n    () => { windows_spawn::SpawnOptions::new().drop_policy(windows_spawn::DropPolicy::KillTree) };\n}\nfn spawn()",
            )
            .replace("    let options =", "    let _accepted_options =")
            .replace(
                "command.spawn_with(options)",
                "command.spawn_with(kill_tree_options!())",
            );
        assert_eq!(
            check(&dead_options),
            vec![
                "macro-in-spawn-item: kill_tree_options",
                "spawn-options: opaque macro kill_tree_options",
            ],
            "dead accepted options must not validate a KillTree macro passed to spawn_with"
        );

        let wrong_local = FIXTURE
            .replace("    let options =", "    let _accepted_options =")
            .replace(
                "    command.spawn_with(options)?;",
                "    let actual = windows_spawn::SpawnOptions::new()\n        .drop_policy(windows_spawn::DropPolicy::KillTree);\n    command.spawn_with(actual)?;",
            );
        assert_eq!(
            check(&wrong_local),
            vec![
                "forbidden-drop-policy: KillTree",
                "spawn-options-creation-flags-count: 0",
                "spawn-options-drop-policy: windows_spawn::DropPolicy::KillTree",
            ],
            "dead accepted options must not validate a different local passed to spawn_with"
        );

        let opaque_builder = FIXTURE.replace(
            "command.spawn_with(options)",
            "command.spawn_with(build_options())",
        );
        assert_eq!(
            check(&opaque_builder),
            vec![
                "spawn-options: opaque expression",
                "unresolved-path: build_options",
            ],
            "a helper's return value is opaque at the SpawnOptions sink"
        );

        let command_macro = FIXTURE.replace(
            "    command.env_clear();",
            "    inspect_command!(command);\n    command.env_clear();",
        );
        assert_eq!(
            check(&command_macro),
            vec!["macro-in-spawn-item: inspect_command"],
            "a macro receiving the command value must be rejected as an opaque escape"
        );
    }

    #[test]
    fn a_no_argument_local_macro_cannot_capture_the_spawned_command() {
        let quickstart = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/quickstart.rs");
        let source = std::fs::read_to_string(&quickstart)
            .unwrap_or_else(|error| panic!("reading {}: {error}", quickstart.display()));
        let command = "        let mut command = windows_spawn::Command::new(&daemon);";
        let stderr = "        command.stderr(windows_spawn::Stdio::from(stderr));";
        assert_eq!(source.matches(command).count(), 1, "command fixture drift");
        assert_eq!(source.matches(stderr).count(), 1, "stderr fixture drift");
        let mutant = source
            .replacen(
                command,
                &format!(
                    "{command}\n        macro_rules! replace_stdin_with_third_log {{\n            () => {{\n                command.stdin(windows_spawn::Stdio::from(\n                    log.try_clone().map_err(|source| Error::Log {{\n                        path: log_path.clone(),\n                        source,\n                    }})?,\n                ))\n            }};\n        }}"
                ),
                1,
            )
            .replacen(
                stderr,
                &format!("{stderr}\n        replace_stdin_with_third_log!();"),
                1,
            );

        assert_eq!(
            check(&mutant),
            vec![
                "macro-in-spawn-item: macro_rules",
                "macro-in-spawn-item: replace_stdin_with_third_log",
            ],
            "a local macro definition and its no-argument invocation must both fail closed"
        );
    }
}

// The login's item, with exactly its admitted calls.
// Quickstart's Windows item keeps the profile above. The gate reads every call and every
// path in the route file: each method call is placed in the chain it belongs to, whatever its
// shape (a statement per call, a builder chain, a closure), each chain rooted at the command is
// rendered call by call with its arguments and compared with the admitted list, and every path
// that names a process capability must be one the item is admitted, the number of times it is
// admitted, in call position.
mod route_items {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::Write as _;
    use std::path::Path;

    use syn::visit::{self, Visit};
    use syn::{Expr, ExprMethodCall, ItemFn, Lit, Pat, Stmt, UseTree};

    /// A path naming any of these is a process capability.
    const SENSITIVE_SEGMENTS: &[&str] = &[
        "process",
        "windows_spawn",
        "Command",
        "CommandExt",
        "Stdio",
        "SpawnOptions",
        "CreationFlags",
        "DropPolicy",
        "Child",
        "Job",
    ];

    /// A method call with one of these names that is not one of the admitted calls is a stray.
    const SENSITIVE_METHODS: &[&str] = &[
        "arg",
        "args",
        "arg0",
        "env",
        "envs",
        "env_clear",
        "env_remove",
        "current_dir",
        "stdin",
        "stdout",
        "stderr",
        "process_group",
        "pre_exec",
        "uid",
        "gid",
        "groups",
        "spawn",
        "spawn_with",
        "spawn_suspended",
        "spawn_suspended_with",
        "status",
        "status_with",
        "output",
        "output_with",
        "exec",
        "raw_arg",
        "arg_handle",
        "env_handle",
        "creation_flags",
        "drop_policy",
        "job",
        "parent_process",
        "pseudoconsole",
        "mitigation",
        "kill",
        "wait",
        "wait_with_output",
        "try_wait",
    ];

    fn path_text(path: &syn::Path) -> String {
        path.segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::")
    }

    fn sensitive(path: &str) -> bool {
        path.split("::")
            .any(|segment| SENSITIVE_SEGMENTS.contains(&segment))
    }

    fn strip(mut expression: &Expr) -> &Expr {
        loop {
            expression = match expression {
                Expr::Try(tried) => &tried.expr,
                Expr::Paren(paren) => &paren.expr,
                Expr::Reference(reference) => &reference.expr,
                Expr::Group(group) => &group.expr,
                _ => return expression,
            };
        }
    }

    fn ident(expression: &Expr) -> Option<String> {
        match strip(expression) {
            Expr::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 => {
                Some(path.path.segments[0].ident.to_string())
            }
            _ => None,
        }
    }

    /// Where a method call's chain starts, and how many calls come before it in that chain.
    fn base(receiver: &Expr) -> (&Expr, usize) {
        match strip(receiver) {
            Expr::MethodCall(call) => {
                let (base, before) = base(&call.receiver);
                (base, before + 1)
            }
            other => (other, 0),
        }
    }

    fn render(expression: &Expr) -> String {
        match strip(expression) {
            Expr::Path(path) if path.qself.is_none() => path_text(&path.path),
            Expr::Field(field) => match &field.member {
                syn::Member::Named(name) => format!("{}.{name}", render(&field.base)),
                syn::Member::Unnamed(_) => "?".to_owned(),
            },
            Expr::Lit(literal) => match &literal.lit {
                Lit::Str(text) => format!("{:?}", text.value()),
                Lit::Int(number) => number.base10_digits().to_owned(),
                _ => "?".to_owned(),
            },
            Expr::Call(call) if call.args.is_empty() => format!("{}()", render(&call.func)),
            Expr::Binary(binary) if matches!(binary.op, syn::BinOp::BitOr(_)) => {
                format!("{}|{}", render(&binary.left), render(&binary.right))
            }
            _ => "?".to_owned(),
        }
    }

    fn call_text(call: &ExprMethodCall) -> String {
        let arguments: Vec<String> = call.args.iter().map(render).collect();
        format!("{}({})", call.method, arguments.join(","))
    }

    /// Everything the gate reads in one function.
    #[derive(Default)]
    struct Facts<'ast> {
        locals: BTreeMap<String, Vec<Option<&'ast Expr>>>,
        methods: Vec<&'ast ExprMethodCall>,
        callees: BTreeSet<usize>,
        paths: Vec<(String, bool)>,
        idents: BTreeMap<String, usize>,
        qualified: usize,
        macros: Vec<String>,
        items: usize,
    }

    impl<'ast> Visit<'ast> for Facts<'ast> {
        fn visit_local(&mut self, local: &'ast syn::Local) {
            let mut pattern = &local.pat;
            if let Pat::Type(typed) = pattern {
                pattern = &typed.pat;
            }
            let name = match pattern {
                Pat::Ident(name) => name.ident.to_string(),
                _ => "_".to_owned(),
            };
            self.locals
                .entry(name)
                .or_default()
                .push(local.init.as_ref().map(|init| init.expr.as_ref()));
            visit::visit_local(self, local);
        }

        fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
            self.methods.push(call);
            visit::visit_expr_method_call(self, call);
        }

        fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
            if let Expr::Path(path) = call.func.as_ref() {
                self.callees.insert(std::ptr::from_ref(path) as usize);
            }
            visit::visit_expr_call(self, call);
        }

        fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
            if path.qself.is_some() {
                self.qualified += 1;
            }
            let text = path_text(&path.path);
            if path.path.segments.len() == 1 {
                *self.idents.entry(text.clone()).or_default() += 1;
            }
            let callee = self.callees.contains(&(std::ptr::from_ref(path) as usize));
            self.paths.push((text, callee));
            visit::visit_expr_path(self, path);
        }

        fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
            if path.qself.is_some() {
                self.qualified += 1;
            }
            self.paths.push((path_text(&path.path), false));
            visit::visit_type_path(self, path);
        }

        fn visit_macro(&mut self, invocation: &'ast syn::Macro) {
            self.macros.push(path_text(&invocation.path));
            visit::visit_macro(self, invocation);
        }

        fn visit_item(&mut self, item: &'ast syn::Item) {
            self.items += 1;
            visit::visit_item(self, item);
        }
    }

    /// A chain's start and its calls, each with how many calls come before it.
    type Unordered<'ast> = (&'ast Expr, Vec<(usize, &'ast ExprMethodCall)>);

    /// The chains of method calls in `facts`, keyed by the address of the expression each starts
    /// from, each in call order.
    fn chains<'ast>(
        facts: &Facts<'ast>,
    ) -> BTreeMap<usize, (&'ast Expr, Vec<&'ast ExprMethodCall>)> {
        let mut found: BTreeMap<usize, Unordered<'ast>> = BTreeMap::new();
        for call in &facts.methods {
            let (start, before) = base(&call.receiver);
            found
                .entry(std::ptr::from_ref(start) as usize)
                .or_insert_with(|| (start, Vec::new()))
                .1
                .push((before, call));
        }
        found
            .into_iter()
            .map(|(key, (start, mut calls))| {
                calls.sort_by_key(|(before, _)| *before);
                (
                    key,
                    (start, calls.into_iter().map(|(_, call)| call).collect()),
                )
            })
            .collect()
    }

    /// What one command value may do: its constructor, the one argument that names the program,
    /// every call made on it up to and including the call that starts the program, and every
    /// call after that on the result.
    struct Profile {
        label: &'static str,
        constructor: &'static str,
        program: &'static str,
        calls: &'static [&'static str],
        terminal: &'static str,
        after: &'static [&'static str],
    }

    const LOGIN: Profile = Profile {
        label: "login",
        constructor: "std::process::Command::new",
        program: "program",
        calls: &[
            "args(managed.plan.args)",
            "args(tokens)",
            "current_dir(home)",
            "current_dir(managed.plan.cwd)",
            "env(\"CODEX_HOME\",managed.home)",
            "env_remove(\"DUDES_SOCK\")",
            "env_remove(\"DUDES_TOKEN\")",
            "exec()",
        ],
        terminal: "exec",
        after: &[],
    };

    const EDITOR: Profile = Profile {
        label: "editor",
        constructor: "std::process::Command::new",
        program: "program",
        calls: &[
            "arg(directory)",
            "args(args)",
            "env_remove(\"DUDES_SOCK\")",
            "env_remove(\"DUDES_TOKEN\")",
            "exec()",
        ],
        terminal: "exec",
        after: &[],
    };

    /// The paths each route item may name, how many times, and whether as a call.
    const LOGIN_PATHS: &[(&str, usize, bool)] = &[("std::process::Command::new", 1, true)];

    /// The capability paths the rest of each route file may name, outside its item.
    const LOGIN_FILE_PATHS: &[&str] = &["std::os::unix::process::CommandExt"];

    /// The one function in `file` that answers `clippy::disallowed_types`.
    fn spawn_item(file: &syn::File) -> Result<&ItemFn, String> {
        let items: Vec<&ItemFn> =
            file.items
                .iter()
                .filter_map(|item| match item {
                    syn::Item::Fn(function) => Some(function),
                    _ => None,
                })
                .filter(|function| {
                    function.attrs.iter().any(|attribute| {
                        attribute.path().is_ident("expect")
                            && attribute.meta.require_list().is_ok_and(|list| {
                                list.tokens.to_string().contains("disallowed_types")
                            })
                    })
                })
                .collect();
        match items.as_slice() {
            [item] => Ok(item),
            other => Err(format!("spawn-items: {}", other.len())),
        }
    }

    /// The single value `name` was bound to in `facts`, when it was bound exactly once.
    fn only<'ast>(facts: &Facts<'ast>, name: &str) -> Option<&'ast Expr> {
        match facts.locals.get(name).map(Vec::as_slice) {
            Some([Some(value)]) => Some(value),
            _ => None,
        }
    }

    /// Whether `expression` calls `path` directly.
    fn calls(expression: &Expr, path: &str) -> bool {
        matches!(expression, Expr::Call(call)
            if matches!(call.func.as_ref(), Expr::Path(callee)
                if callee.qself.is_none() && path_text(&callee.path) == path))
    }

    /// Check one command value against `profile`, and return the calls it admitted.
    fn command<'ast>(
        profile: &Profile,
        facts: &Facts<'ast>,
        chains: &BTreeMap<usize, (&'ast Expr, Vec<&'ast ExprMethodCall>)>,
        violations: &mut BTreeSet<String>,
    ) -> BTreeSet<usize> {
        let label = profile.label;
        let mut admitted = BTreeSet::new();
        let bound: Vec<&String> = facts
            .locals
            .iter()
            .filter(|(_, values)| {
                values
                    .iter()
                    .any(|value| value.is_some_and(|value| calls(value, profile.constructor)))
            })
            .map(|(name, _)| name)
            .collect();
        let [name] = bound.as_slice() else {
            violations.insert(format!("{label} command-bindings: {}", bound.len()));
            return admitted;
        };
        let Some(Expr::Call(new)) = only(facts, name) else {
            violations.insert(format!(
                "{label} command-binding: {name} is bound more than once"
            ));
            return admitted;
        };
        if new.args.len() != 1 || ident(&new.args[0]).as_deref() != Some(profile.program) {
            violations.insert(format!("{label} program: not `{}`", profile.program));
        }
        let rooted: Vec<&Vec<&ExprMethodCall>> = chains
            .values()
            .filter(|(start, _)| ident(start).as_deref() == Some(name.as_str()))
            .map(|(_, calls)| calls)
            .collect();
        if facts.idents.get(name.as_str()).copied().unwrap_or(0) != rooted.len() {
            violations.insert(format!("{label} command-escape: {name}"));
        }
        let mut made = Vec::new();
        let mut after = Vec::new();
        for chain in rooted {
            let mut past_terminal = false;
            for call in chain {
                admitted.insert(std::ptr::from_ref(*call) as usize);
                if past_terminal {
                    after.push(call_text(call));
                } else {
                    made.push(call_text(call));
                    past_terminal = call.method == profile.terminal;
                }
            }
        }
        made.sort();
        if made != profile.calls {
            violations.insert(format!("{label} calls: {made:?}"));
        }
        if after != profile.after {
            violations.insert(format!("{label} after-start: {after:?}"));
        }
        admitted
    }

    /// Every capability path in `facts` against `allowed`: none unlisted, each as often as listed,
    /// and each function one in call position.
    fn census(
        label: &str,
        facts: &Facts<'_>,
        allowed: &[(&str, usize, bool)],
        violations: &mut BTreeSet<String>,
    ) {
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for (path, callee) in &facts.paths {
            if !sensitive(path) {
                continue;
            }
            match allowed.iter().find(|(name, _, _)| name == path) {
                Some((name, _, called)) => {
                    *seen.entry(name).or_default() += 1;
                    if *called && !callee {
                        violations.insert(format!("{label} path not called: {path}"));
                    }
                }
                None => {
                    violations.insert(format!("{label} path: {path}"));
                }
            }
        }
        for (name, count, _) in allowed {
            let found = seen.get(name).copied().unwrap_or(0);
            if found != *count {
                violations.insert(format!("{label} path count: {name} {found}"));
            }
        }
        if facts.qualified != 0 {
            violations.insert(format!("{label} qualified paths: {}", facts.qualified));
        }
        if !facts.macros.is_empty() {
            violations.insert(format!("{label} macros: {:?}", facts.macros));
        }
        if facts.items != 0 {
            violations.insert(format!("{label} nested items: {}", facts.items));
        }
    }

    /// Every sensitive method call in `facts` that no profile admitted.
    fn strays(
        label: &str,
        facts: &Facts<'_>,
        admitted: &BTreeSet<usize>,
        violations: &mut BTreeSet<String>,
    ) {
        for call in &facts.methods {
            let name = call.method.to_string();
            if SENSITIVE_METHODS.contains(&name.as_str())
                && !admitted.contains(&(std::ptr::from_ref(*call) as usize))
            {
                violations.insert(format!("{label} stray call: {}", call_text(call)));
            }
        }
    }

    /// The rest of a route file: no capability path but `allowed`, no `macro_rules!`, and the one
    /// spawn item.
    fn rest_of_file(
        label: &str,
        file: &syn::File,
        item: &ItemFn,
        allowed: &[&str],
        violations: &mut BTreeSet<String>,
    ) {
        struct Rest<'a> {
            item: *const ItemFn,
            paths: Vec<String>,
            macro_rules: usize,
            _file: std::marker::PhantomData<&'a ()>,
        }
        impl<'ast> Visit<'ast> for Rest<'_> {
            fn visit_item_fn(&mut self, function: &'ast ItemFn) {
                if !std::ptr::eq(function, self.item) {
                    visit::visit_item_fn(self, function);
                }
            }
            fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
                if item.mac.path.is_ident("macro_rules") {
                    self.macro_rules += 1;
                }
                visit::visit_item_macro(self, item);
            }
            fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
                flatten(&item.tree, &mut Vec::new(), &mut self.paths);
            }
            fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
                self.paths.push(path_text(&path.path));
                visit::visit_expr_path(self, path);
            }
            fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
                self.paths.push(path_text(&path.path));
                visit::visit_type_path(self, path);
            }
        }
        fn flatten(tree: &UseTree, prefix: &mut Vec<String>, out: &mut Vec<String>) {
            match tree {
                UseTree::Path(path) => {
                    prefix.push(path.ident.to_string());
                    flatten(&path.tree, prefix, out);
                    prefix.pop();
                }
                UseTree::Name(name) => out.push(
                    prefix
                        .iter()
                        .cloned()
                        .chain([name.ident.to_string()])
                        .collect::<Vec<_>>()
                        .join("::"),
                ),
                UseTree::Rename(rename) => out.push(
                    prefix
                        .iter()
                        .cloned()
                        .chain([rename.ident.to_string()])
                        .collect::<Vec<_>>()
                        .join("::"),
                ),
                UseTree::Glob(_) => out.push(format!("{}::*", prefix.join("::"))),
                UseTree::Group(group) => {
                    for item in &group.items {
                        flatten(item, prefix, out);
                    }
                }
            }
        }
        let mut rest = Rest {
            item,
            paths: Vec::new(),
            macro_rules: 0,
            _file: std::marker::PhantomData,
        };
        rest.visit_file(file);
        for path in rest.paths {
            if sensitive(&path) && !allowed.contains(&path.as_str()) {
                violations.insert(format!("{label} file path: {path}"));
            }
        }
        if rest.macro_rules != 0 {
            violations.insert(format!("{label} macro_rules: {}", rest.macro_rules));
        }
    }

    fn login(text: &str) -> Vec<String> {
        let file = syn::parse_file(text).expect("the login parses");
        let mut violations = BTreeSet::new();
        let item = match spawn_item(&file) {
            Ok(item) => item,
            Err(violation) => return vec![violation],
        };
        rest_of_file("login", &file, item, LOGIN_FILE_PATHS, &mut violations);
        let mut facts = Facts::default();
        facts.visit_item_fn(item);
        census("login", &facts, LOGIN_PATHS, &mut violations);
        let chains = chains(&facts);
        let admitted = command(&LOGIN, &facts, &chains, &mut violations);
        strays("login", &facts, &admitted, &mut violations);

        let statements = &item.block.stmts;
        let command_at = statements.iter().position(|statement| {
            matches!(statement, Stmt::Local(local)
                if local.init.as_ref().is_some_and(|init| calls(&init.expr, LOGIN.constructor)))
        });
        let guarded = command_at.is_some_and(|at| {
            statements[..at].iter().any(|statement| {
                let Stmt::Expr(Expr::If(check), _) = statement else {
                    return false;
                };
                let refuses_relative = matches!(check.cond.as_ref(), Expr::Unary(not)
                    if matches!(not.op, syn::UnOp::Not(_))
                        && matches!(strip(&not.expr), Expr::MethodCall(call)
                            if call.method == "is_absolute"
                                && call.args.is_empty()
                                && ident(&call.receiver).as_deref() == Some("program")));
                let returns = check
                    .then_branch
                    .stmts
                    .iter()
                    .any(|inner| matches!(inner, Stmt::Expr(Expr::Return(_), _)));
                refuses_relative && returns && check.else_branch.is_none()
            })
        });
        if !guarded || facts.locals.contains_key("program") {
            violations.insert("login program: not checked absolute before the command".to_owned());
        }
        let constant = |value: &Expr| {
            matches!(value, Expr::Path(path)
                if matches!(path_text(&path.path).as_str(), "CLAUDE_LOGIN" | "CODEX_LOGIN"))
        };
        let tokens = only(&facts, "tokens").is_some_and(|bound| match bound {
            Expr::Match(choice) => choice.arms.iter().all(|arm| constant(&arm.body)),
            other => constant(other),
        });
        if !tokens {
            violations.insert("login args: not one of the constant token lists".to_owned());
        }
        if !violations
            .iter()
            .any(|value| value.starts_with("login calls:"))
            && !managed_login_branch(statements)
        {
            violations
                .insert("login managed plan: not selected from the admitted profile".to_owned());
        }
        violations.into_iter().collect()
    }

    fn editor(text: &str) -> Vec<String> {
        let file = syn::parse_file(text).expect("the editor parses");
        let mut violations = BTreeSet::new();
        let item = match spawn_item(&file) {
            Ok(item) => item,
            Err(violation) => return vec![violation],
        };
        if item.sig.ident != "exec"
            || !item.attrs.iter().any(|attribute| {
                attribute.path().is_ident("cfg")
                    && attribute
                        .meta
                        .require_list()
                        .is_ok_and(|list| list.tokens.to_string() == "unix")
            })
            || super::ban_suppressions(text) != ["expect(clippy::disallowed_types)"]
        {
            violations.insert("editor grant: not the single Unix exec item".to_owned());
        }
        rest_of_file("editor", &file, item, LOGIN_FILE_PATHS, &mut violations);
        let mut facts = Facts::default();
        facts.visit_item_fn(item);
        census("editor", &facts, LOGIN_PATHS, &mut violations);
        let chains = chains(&facts);
        let admitted = command(&EDITOR, &facts, &chains, &mut violations);
        strays("editor", &facts, &admitted, &mut violations);
        if ["program", "args", "directory"]
            .iter()
            .any(|name| facts.locals.contains_key(*name))
        {
            violations.insert("editor operands: rebound inside exec".to_owned());
        }
        let forms: Vec<_> = item
            .block
            .stmts
            .iter()
            .map(|statement| match statement {
                Stmt::Local(local)
                    if local
                        .init
                        .as_ref()
                        .is_some_and(|init| calls(&init.expr, EDITOR.constructor)) =>
                {
                    "new(program)".to_owned()
                }
                Stmt::Expr(Expr::MethodCall(call), _)
                    if ident(&call.receiver).as_deref() == Some("command") =>
                {
                    call_text(call)
                }
                _ => "?".to_owned(),
            })
            .collect();
        if forms
            != [
                "new(program)",
                "args(args)",
                "arg(directory)",
                "env_remove(\"DUDES_TOKEN\")",
                "env_remove(\"DUDES_SOCK\")",
                "exec()",
            ]
            || !matches!(item.block.stmts.last(), Some(Stmt::Expr(Expr::MethodCall(call), None)) if call.method == "exec")
        {
            violations.insert("editor body: not the admitted ordered handoff".to_owned());
        }
        violations.into_iter().collect()
    }

    fn path_lookups(name: &str, text: &str) -> Vec<String> {
        struct Lookups<'a> {
            file: &'a str,
            function: String,
            violations: Vec<String>,
        }
        impl<'ast> Visit<'ast> for Lookups<'_> {
            fn visit_item_fn(&mut self, function: &'ast ItemFn) {
                let previous =
                    std::mem::replace(&mut self.function, function.sig.ident.to_string());
                visit::visit_item_fn(self, function);
                self.function = previous;
            }
            fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
                if let Expr::Path(path) = call.func.as_ref() {
                    let name = path.path.segments.last().unwrap().ident.to_string();
                    let reads_path = matches!(name.as_str(), "var" | "var_os")
                        && call
                            .args
                            .first()
                            .is_some_and(|argument| render(argument) == "\"PATH\"");
                    if (name == "split_paths" || reads_path)
                        && !matches!(
                            (self.file, self.function.as_str()),
                            ("editor.rs", "resolve_configured_program") | ("setup.rs", "on_path")
                        )
                    {
                        self.violations
                            .push(format!("{}::{}: {name}", self.file, self.function));
                    }
                }
                visit::visit_expr_call(self, call);
            }
            fn visit_use_tree(&mut self, tree: &'ast UseTree) {
                let name = match tree {
                    UseTree::Name(name) => Some(&name.ident),
                    UseTree::Rename(name) => Some(&name.ident),
                    _ => None,
                };
                if name.is_some_and(|name| name == "split_paths") {
                    self.violations
                        .push(format!("{}: split_paths import", self.file));
                }
                visit::visit_use_tree(self, tree);
            }
        }
        let file = syn::parse_file(text).expect("the lookup source parses");
        let mut found = Lookups {
            file: name,
            function: String::new(),
            violations: Vec::new(),
        };
        found.visit_file(&file);
        found.violations
    }

    fn managed_login_branch(statements: &[Stmt]) -> bool {
        let forms = |body: &syn::Block| {
            body.stmts
                .iter()
                .map(|statement| match statement {
                    Stmt::Expr(Expr::MethodCall(call), _)
                        if ident(&call.receiver).as_deref() == Some("command") =>
                    {
                        call_text(call)
                    }
                    _ => "?".to_owned(),
                })
                .collect::<Vec<_>>()
        };
        statements.iter().any(|statement| {
            let Stmt::Expr(Expr::If(branch), _) = statement else { return false; };
            let Expr::Let(binding) = branch.cond.as_ref() else { return false; };
            let Pat::TupleStruct(pattern) = binding.pat.as_ref() else { return false; };
            let Some((_, otherwise)) = &branch.else_branch else { return false; };
            let Expr::Block(otherwise) = otherwise.as_ref() else { return false; };
            ident(&binding.expr).as_deref() == Some("managed")
                && pattern.path.is_ident("Some")
                && matches!(pattern.elems.first(), Some(Pat::Ident(name)) if name.ident == "managed")
                && pattern.elems.len() == 1
                && forms(&branch.then_branch) == ["args(managed.plan.args)", "env(\"CODEX_HOME\",managed.home)", "current_dir(managed.plan.cwd)"]
                && forms(&otherwise.block) == ["args(tokens)", "current_dir(home)"]
        })
    }

    fn source(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src").join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()))
    }

    fn mutant(name: &str, from: &str, to: &str) -> String {
        let text = source(name);
        assert_eq!(
            text.matches(from).count(),
            1,
            "the mutation target drifted: {from}"
        );
        text.replacen(from, to, 1)
    }

    #[test]
    fn the_login_makes_only_its_admitted_calls() {
        assert_eq!(login(&source("login.rs")), Vec::<String>::new());
    }

    #[test]
    fn the_editor_makes_only_its_admitted_calls() {
        assert_eq!(editor(&source("editor.rs")), Vec::<String>::new());
    }

    #[test]
    fn editor_item_mutants_are_rejected_without_widening_the_grant() {
        assert!(editor(&source("editor.rs")).is_empty());
        for (name, from, to) in [
            (
                "extra spawn",
                "    command.exec()",
                "    command.spawn();\n    command.exec()",
            ),
            (
                "composed interpreter",
                "Command::new(program)",
                "Command::new(\"/bin/sh\")",
            ),
            (
                "extra operand",
                "    command.arg(directory);",
                "    command.arg(directory);\n    command.arg(\"extra\");",
            ),
            (
                "credential retained",
                "    command.env_remove(\"DUDES_TOKEN\");\n",
                "",
            ),
            (
                "exec reordered",
                "    command.env_remove(\"DUDES_SOCK\");\n    command.exec()",
                "    command.exec();\n    command.env_remove(\"DUDES_SOCK\");",
            ),
            (
                "broad allowance",
                "clippy::disallowed_types,",
                "clippy::all,",
            ),
        ] {
            let found = editor(&mutant("editor.rs", from, to));
            assert!(!found.is_empty(), "surviving {name}");
            writeln!(std::io::stdout().lock(), "{name}: {found:?}").unwrap();
        }
        let another = format!(
            "{}\nfn another(program: &Path) {{ std::process::Command::new(program).spawn(); }}\n",
            source("editor.rs")
        );
        assert!(!editor(&another).is_empty());
    }

    #[test]
    fn path_lookup_stays_in_the_editor_resolver_and_existing_setup_probe() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        super::walk(&root, &mut |path| {
            if path.extension().is_some_and(|extension| extension == "rs") {
                let source = std::fs::read_to_string(path).unwrap();
                assert!(
                    path_lookups(path.file_name().unwrap().to_str().unwrap(), &source).is_empty(),
                    "{}",
                    path.display()
                );
            }
        });
        for name in ["editor.rs", "run.rs"] {
            let mutant = format!(
                "{}\nfn another_lookup() {{ let path = std::env::var_os(\"PATH\").unwrap(); let _ = std::env::split_paths(&path); }}\n",
                source(name)
            );
            let rejected = path_lookups(name, &mutant);
            assert_eq!(rejected.len(), 2, "{name}: {rejected:?}");
            writeln!(
                std::io::stdout().lock(),
                "PATH lookup outside resolver in {name}: {rejected:?}"
            )
            .unwrap();
        }
        let aliased =
            "use std::env::split_paths as elsewhere; fn another() { let _ = elsewhere(\"x\"); }";
        assert_eq!(
            path_lookups("run.rs", aliased),
            ["run.rs: split_paths import"]
        );
    }

    #[test]
    fn managed_login_cannot_replace_the_plan_or_reuse_the_callers_home() {
        assert!(login(&source("login.rs")).is_empty());
        for (from, to) in [
            ("command.args(&managed.plan.args);", "command.args(tokens);"),
            (
                "command.current_dir(&managed.plan.cwd);",
                "command.current_dir(home);",
            ),
            (
                "command.env(\"CODEX_HOME\", &managed.home);",
                "command.env(\"CODEX_HOME\", home);",
            ),
        ] {
            let found = login(&mutant("login.rs", from, to));
            assert!(
                found.iter().any(|value| value.starts_with("login calls:")),
                "{found:?}"
            );
        }
        assert_eq!(
            login(&mutant(
                "login.rs",
                "if let Some(managed) = managed {",
                "if let Some(managed) = None::<&ManagedCodexLogin> {"
            )),
            ["login managed plan: not selected from the admitted profile"]
        );
    }

    /// Every planted mutant of the login item, with the exact violations it must print.
    fn arms() -> Vec<(&'static str, Vec<String>, Vec<&'static str>)> {
        vec![
            (
                "a token removed, then set again in the same chain",
                login(&mutant(
                    "login.rs",
                    "    command.env_remove(\"DUDES_TOKEN\");\n",
                    "    command.env_remove(\"DUDES_TOKEN\").env(\"DUDES_TOKEN\", \"leak\");\n",
                )),
                vec![
                    "login calls: [\"args(managed.plan.args)\", \"args(tokens)\", \"current_dir(home)\", \"current_dir(managed.plan.cwd)\", \"env(\\\"CODEX_HOME\\\",managed.home)\", \"env(\\\"DUDES_TOKEN\\\",\\\"leak\\\")\", \"env_remove(\\\"DUDES_SOCK\\\")\", \"env_remove(\\\"DUDES_TOKEN\\\")\", \"exec()\"]",
                ],
            ),
            (
                "the absolute check made a no-op",
                login(&mutant(
                    "login.rs",
                    "    if !program.is_absolute() {\n        return std::io::Error::new(\n            std::io::ErrorKind::InvalidInput,\n            \"the login program is not an absolute path\",\n        );\n    }\n",
                    "    let _ = program.is_absolute();\n",
                )),
                vec!["login program: not checked absolute before the command"],
            ),
            (
                "login builds its argv",
                login(&mutant(
                    "login.rs",
                    "command.args(tokens);",
                    "command.args([\"auth\", \"login\"]);",
                )),
                vec![
                    "login calls: [\"args(?)\", \"args(managed.plan.args)\", \"current_dir(home)\", \"current_dir(managed.plan.cwd)\", \"env(\\\"CODEX_HOME\\\",managed.home)\", \"env_remove(\\\"DUDES_SOCK\\\")\", \"env_remove(\\\"DUDES_TOKEN\\\")\", \"exec()\"]",
                ],
            ),
            (
                "login adds a token",
                login(&mutant(
                    "login.rs",
                    "        Engine::Codex => CODEX_LOGIN,\n    };",
                    "        Engine::Codex => &[\"login\", \"--api-key\"],\n    };",
                )),
                vec!["login args: not one of the constant token lists"],
            ),
            (
                "login runs in the repository",
                login(&mutant(
                    "login.rs",
                    "command.current_dir(home);",
                    "command.current_dir(\"/\");",
                )),
                vec![
                    "login calls: [\"args(managed.plan.args)\", \"args(tokens)\", \"current_dir(\\\"/\\\")\", \"current_dir(managed.plan.cwd)\", \"env(\\\"CODEX_HOME\\\",managed.home)\", \"env_remove(\\\"DUDES_SOCK\\\")\", \"env_remove(\\\"DUDES_TOKEN\\\")\", \"exec()\"]",
                ],
            ),
            (
                "login keeps DUDES_TOKEN",
                login(&mutant(
                    "login.rs",
                    "    command.env_remove(\"DUDES_TOKEN\");\n",
                    "",
                )),
                vec![
                    "login calls: [\"args(managed.plan.args)\", \"args(tokens)\", \"current_dir(home)\", \"current_dir(managed.plan.cwd)\", \"env(\\\"CODEX_HOME\\\",managed.home)\", \"env_remove(\\\"DUDES_SOCK\\\")\", \"exec()\"]",
                ],
            ),
        ]
    }

    #[test]
    fn every_route_item_mutant_prints_its_violations() {
        for (name, found, expected) in arms() {
            assert_eq!(found, expected, "{name}");
        }
    }
}

/// `src/runner.rs` is the one place `dudes upgrade` creates a process, and it makes only the calls
/// that run an absolute program with a cleared environment, typed arguments and no standard input.
mod runner_item {
    use std::path::Path;

    use syn::visit::{self, Visit};
    use syn::{Expr, ExprCall, ExprMethodCall};

    #[derive(Default)]
    struct Calls {
        methods: Vec<String>,
        paths: Vec<String>,
        programs: Vec<String>,
    }

    impl<'ast> Visit<'ast> for Calls {
        fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
            self.methods.push(call.method.to_string());
            visit::visit_expr_method_call(self, call);
        }

        fn visit_expr_call(&mut self, call: &'ast ExprCall) {
            if let Expr::Path(path) = &*call.func {
                let name = path
                    .path
                    .segments
                    .iter()
                    .map(|segment| segment.ident.to_string())
                    .collect::<Vec<_>>()
                    .join("::");
                if name.ends_with("Command::new") {
                    self.programs.push(
                        call.args
                            .iter()
                            .map(|arg| match arg {
                                Expr::Path(path) => path
                                    .path
                                    .get_ident()
                                    .map_or_else(|| "?".to_owned(), ToString::to_string),
                                _ => "?".to_owned(),
                            })
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                }
                self.paths.push(name);
            }
            visit::visit_expr_call(self, call);
        }
    }

    fn violations(text: &str) -> Vec<String> {
        let file = syn::parse_file(text).expect("the runner parses");
        let mut calls = Calls::default();
        calls.visit_file(&file);
        calls.methods.sort();
        calls.paths.sort();
        let mut found = Vec::new();
        let methods = [
            "and_then",
            "args",
            "env",
            "env_clear",
            "expose_value",
            "id",
            "join",
            "join",
            "kill",
            "map",
            "map",
            "name",
            "ok",
            "process_group",
            "read_to_end",
            "spawn",
            "stderr",
            "stdin",
            "stdout",
            "success",
            "take",
            "take",
            "try_wait",
            "unwrap_or_default",
            "unwrap_or_default",
            "wait",
        ];
        if calls.methods != methods {
            found.push(format!("methods: {:?}", calls.methods));
        }
        let paths = [
            "Box::new",
            "Box::new",
            "Duration::from_millis",
            "Duration::from_millis",
            "Duration::from_secs",
            "Err",
            "Instant::now",
            "Instant::now",
            "Instant::now",
            "Instant::now",
            "Ok",
            "Stdio::null",
            "Stdio::piped",
            "Stdio::piped",
            "Vec::new",
            "drain",
            "drain",
            "i32::try_from",
            "io::Error::new",
            "kill_process_group",
            "kill_process_group",
            "std::process::Command::new",
            "std::thread::sleep",
            "std::thread::sleep",
            "std::thread::spawn",
            "stop_group",
        ];
        if calls.paths != paths {
            found.push(format!("calls: {:?}", calls.paths));
        }
        if calls.programs != ["program"] {
            found.push(format!("program: {:?}", calls.programs));
        }
        found
    }

    fn source() -> String {
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/runner.rs"))
            .expect("the runner source")
    }

    #[test]
    fn the_runner_makes_only_its_admitted_calls() {
        assert_eq!(violations(&source()), Vec::<String>::new());
    }

    #[test]
    fn runner_mutants_are_rejected() {
        let text = source();
        for (name, from, to) in [
            (
                "a second spawn",
                "let mut child = command.spawn()?;",
                "let _first = command.spawn()?;\n    let mut child = command.spawn()?;",
            ),
            ("inherited environment", "    command.env_clear();\n", ""),
            (
                "a composed program",
                "Command::new(program)",
                "Command::new(\"/usr/bin/true\")",
            ),
            (
                "an inherited stdin",
                "    command.stdin(Stdio::null());\n",
                "",
            ),
            (
                "a process group of its own",
                "    command.process_group(0);\n",
                "",
            ),
        ] {
            assert_eq!(
                text.matches(from).count(),
                1,
                "{name}: the mutation target drifted"
            );
            assert!(
                !violations(&text.replacen(from, to, 1)).is_empty(),
                "surviving {name}"
            );
        }
    }
}

/// `src/opener.rs` is the one place `dudes open` creates a process. It starts the operator's
/// opener with an absolute program, the link as its one argument, a cleared environment, no
/// standard streams and a process group of its own, and it never signals it: the browser the opener
/// starts must outlive this command.
mod opener_item {
    use std::path::Path;

    use syn::visit::{self, Visit};
    use syn::{Expr, ExprCall, ExprMethodCall, ItemConst, ItemFn, LitStr};

    /// A method with one of these names creates, feeds or ends a process.
    const SENSITIVE: &[&str] = &[
        "arg",
        "args",
        "arg0",
        "env",
        "envs",
        "env_clear",
        "env_remove",
        "current_dir",
        "stdin",
        "stdout",
        "stderr",
        "process_group",
        "pre_exec",
        "spawn",
        "status",
        "output",
        "exec",
        "kill",
        "wait",
        "wait_with_output",
        "try_wait",
    ];

    /// The string literals under one item.
    #[derive(Default)]
    struct Lits(Vec<String>);

    impl<'ast> Visit<'ast> for Lits {
        fn visit_lit_str(&mut self, lit: &'ast LitStr) {
            self.0.push(lit.value());
        }
    }

    #[derive(Default)]
    struct Calls {
        methods: Vec<String>,
        paths: Vec<String>,
        programs: Vec<String>,
        absolute_checks: usize,
        env_names: Vec<String>,
        ssh_names: Vec<String>,
        openers: Vec<String>,
    }

    impl<'ast> Visit<'ast> for Calls {
        fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
            let name = call.method.to_string();
            if name == "is_absolute" {
                self.absolute_checks += 1;
            }
            if SENSITIVE.contains(&name.as_str()) {
                self.methods.push(name);
            }
            visit::visit_expr_method_call(self, call);
        }

        fn visit_item_const(&mut self, item: &'ast ItemConst) {
            if item.ident == "OPENER_ENV" {
                let mut lits = Lits::default();
                lits.visit_expr(&item.expr);
                self.env_names = lits.0;
            }
            visit::visit_item_const(self, item);
        }

        fn visit_item_fn(&mut self, item: &'ast ItemFn) {
            let target = match item.sig.ident.to_string().as_str() {
                "over_ssh" => Some(&mut self.ssh_names),
                "system_opener" => Some(&mut self.openers),
                _ => None,
            };
            if let Some(target) = target {
                let mut lits = Lits::default();
                lits.visit_block(&item.block);
                *target = lits.0;
            }
            visit::visit_item_fn(self, item);
        }

        fn visit_expr_call(&mut self, call: &'ast ExprCall) {
            if let Expr::Path(path) = &*call.func {
                let name = path
                    .path
                    .segments
                    .iter()
                    .map(|segment| segment.ident.to_string())
                    .collect::<Vec<_>>()
                    .join("::");
                if name.ends_with("Command::new") {
                    self.programs.push(
                        call.args
                            .iter()
                            .map(|arg| match arg {
                                Expr::Path(path) => path
                                    .path
                                    .get_ident()
                                    .map_or_else(|| "?".to_owned(), ToString::to_string),
                                _ => "?".to_owned(),
                            })
                            .collect::<Vec<_>>()
                            .join(","),
                    );
                }
                if name.contains("Command") || name.contains("Stdio") || name.contains("process") {
                    self.paths.push(name);
                }
            }
            visit::visit_expr_call(self, call);
        }
    }

    fn violations(text: &str) -> Vec<String> {
        let file = syn::parse_file(text).expect("the opener parses");
        let mut calls = Calls::default();
        calls.visit_file(&file);
        calls.methods.sort();
        calls.paths.sort();
        let mut found = Vec::new();
        let methods = [
            "arg",
            "env",
            "env_clear",
            "process_group",
            "spawn",
            "stderr",
            "stdin",
            "stdout",
            "try_wait",
        ];
        if calls.methods != methods {
            found.push(format!("methods: {:?}", calls.methods));
        }
        let paths = [
            "Stdio::null",
            "Stdio::null",
            "Stdio::null",
            "std::process::Command::new",
        ];
        if calls.paths != paths {
            found.push(format!("calls: {:?}", calls.paths));
        }
        if calls.programs != ["program"] {
            found.push(format!("program: {:?}", calls.programs));
        }
        // The environment the opener is given is exactly this list, and nothing else is copied in.
        let env_names = [
            "HOME",
            "PATH",
            "LANG",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "XAUTHORITY",
            "XDG_RUNTIME_DIR",
            "XDG_CURRENT_DESKTOP",
            "XDG_SESSION_TYPE",
            "DBUS_SESSION_BUS_ADDRESS",
            "BROWSER",
        ];
        if calls.env_names != env_names {
            found.push(format!("environment names: {:?}", calls.env_names));
        }
        if calls.absolute_checks != 1 {
            found.push(format!("is_absolute checks: {}", calls.absolute_checks));
        }
        if calls.ssh_names != ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"] {
            found.push(format!("ssh names: {:?}", calls.ssh_names));
        }
        let programs = [
            "/usr/bin/open",
            "/usr/bin/xdg-open",
            "/usr/local/bin/xdg-open",
            "/bin/xdg-open",
        ];
        if calls.openers != programs {
            found.push(format!("system openers: {:?}", calls.openers));
        }
        let launch = text
            .split("pub(crate) fn launch")
            .nth(1)
            .and_then(|rest| rest.split("\n}\n").next())
            .unwrap_or_default();
        let skip = launch.find("if over_ssh() {");
        let absolute = launch.find("Some(path) if path.is_absolute() => path.to_path_buf(),");
        let run = launch.find("run(&program, url)");
        if !matches!((skip, absolute, run), (Some(skip), Some(absolute), Some(run))
            if skip < absolute && absolute < run)
        {
            found.push("launch: not skip, then absolute check, then run".to_owned());
        }
        found
    }

    fn source() -> String {
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/opener.rs"))
            .expect("the opener source")
    }

    #[test]
    fn the_opener_makes_only_its_admitted_calls() {
        assert_eq!(violations(&source()), Vec::<String>::new());
    }

    #[test]
    fn opener_mutants_are_rejected() {
        let text = source();
        for (name, from, to) in [
            (
                "a second spawn",
                "    command.spawn()\n",
                "    let _first = command.spawn()?;\n    command.spawn()\n",
            ),
            ("inherited environment", "    command.env_clear();\n", ""),
            (
                "a composed program",
                "Command::new(program)",
                "Command::new(\"/usr/bin/true\")",
            ),
            (
                "an inherited stdin",
                "command.stdin(Stdio::null());",
                "command.stdin(Stdio::inherit());",
            ),
            (
                "an inherited stdout",
                "command.stdout(Stdio::null());",
                "command.stdout(Stdio::piped());",
            ),
            (
                "a process group of its own removed",
                "    command.process_group(0);\n",
                "",
            ),
            (
                "a second argument",
                "    command.arg(url);\n",
                "    command.arg(url);\n    command.arg(\"-c\");\n",
            ),
            (
                "a name added to the environment list",
                "    \"BROWSER\",\n];",
                "    \"BROWSER\",\n    \"DUDES_TOKEN\",\n];",
            ),
            (
                "the whole environment copied back in",
                "    command.arg(url);\n",
                "    for (key, value) in std::env::vars() {\n        command.env(key, value);\n    }\n    command.arg(url);\n",
            ),
            (
                "a relative DUDES_OPENER admitted",
                "Some(path) if path.is_absolute() => path.to_path_buf(),",
                "Some(path) => path.to_path_buf(),",
            ),
            (
                "the SSH skip removed",
                "    if over_ssh() {",
                "    if false {",
            ),
            ("an SSH name dropped", "\"SSH_CLIENT\", ", ""),
            (
                "a signal to the opener",
                "            Ok(None) => return Launch::Opened,\n",
                "            Ok(None) => {\n                let _ = child.kill();\n                return Launch::Opened;\n            }\n",
            ),
        ] {
            assert_eq!(
                text.matches(from).count(),
                1,
                "{name}: the mutation target drifted"
            );
            assert!(
                !violations(&text.replacen(from, to, 1)).is_empty(),
                "surviving {name}"
            );
        }
    }
}

/// Only the upgrade command and the one `SystemRunner` in `run.rs` may reach the process runner.
#[test]
fn only_upgrade_and_its_system_runner_reference_the_runner() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut users = Vec::new();
    walk(&src, &mut |path| {
        if path.extension().is_none_or(|ext| ext != "rs") || path.ends_with("runner.rs") {
            return;
        }
        let text = std::fs::read_to_string(path).expect("a source file");
        let (_, code) = masked(&text);
        if code.contains("runner::") {
            users.push(path.strip_prefix(&src).unwrap().display().to_string());
        }
    });
    users.sort();
    assert_eq!(users, ["run.rs", "upgrade.rs"]);
}
