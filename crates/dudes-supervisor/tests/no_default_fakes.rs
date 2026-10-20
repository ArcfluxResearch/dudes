//! The fakes are not in a default build of this crate, and no build in this workspace asks for
//! them.
//!
//! What is at stake is **invariant 8**. `FakeRng` fakes the entropy source for the per-agent hook
//! token, and that token is the capability that separates one agent from another on the socket. A
//! daemon that mints from a counter has agents a local process can impersonate in one guess, so
//! every check here is ultimately about that one invariant and says so where it fails.
//!
//! # What is actually enforced, and by what
//!
//! The enforcement is the **compiler**, not this file. `src/lib.rs` declares the module
//! `#[cfg(any(test, feature = "testing"))]`, so `cargo build -p dudes-supervisor` does not compile
//! `src/testing.rs` and a dependent that writes `dudes_supervisor::testing::FakeRng` without the
//! feature fails to resolve the path. Measured on this tree with one scratch crate outside the
//! workspace, depending on `dudes-supervisor` and doing nothing but `use
//! dudes_supervisor::testing::FakeRng` and printing three tokens. With the attribute deleted from
//! `src/lib.rs` and **no feature named in its manifest** it builds and prints
//! `AAAA…1`, `AAAA…2`, `AAAA…3` — three well-formed 43-character agent tokens. With the attribute
//! in place and the same manifest it fails:
//!
//! ```text
//! error[E0432]: unresolved import `dudes_supervisor::testing`
//! note: found an item that was configured out
//!   --> crates/dudes-supervisor/src/lib.rs:54:9
//!    | #[cfg(any(test, feature = "testing"))]
//!    |          --------------------------- the item is gated here
//! ```
//!
//! and it compiles and prints again the moment `features = ["testing"]` goes into that manifest.
//!
//! This file exists because that arrangement is four edits away from being undone, and each of the
//! four is silent — the suite stays green, `clippy` stays clean, and a counted token mint is back
//! within reach of every daemon that links the crate:
//!
//! 1. the attribute is dropped from the declaration in `src/lib.rs`;
//! 2. a `default` table is added to `Cargo.toml` naming the feature;
//! 3. a dependent turns the feature on from a **non-dev** dependency table;
//! 4. **a dependent's own `default` reaches the feature** — a `default = ["dudes-supervisor/testing"]`
//!    in a sibling's manifest, or a `default` that names a feature that names it.
//!
//! Each is a check below, and each check has its own `*_catches_a_planted_regression` control that
//! plants the regression and asserts the same function the check runs reports it. One control per
//! scan rather than one for the file: a single control going red would say only that *something* is
//! unproven, and telling one silent regression from another is this file's whole job.
//!
//! All four were then planted into the **real** manifests that would carry them — the attribute
//! deleted from `src/lib.rs`, a `default = ["testing"]` added to this crate's `[features]`,
//! `bins/duded`'s shipping entry given `features = ["testing"]`, and a
//! `default = ["dudes-supervisor/testing"]` added to `crates/dudes-host` — and each reddened the
//! check that owns it.
//!
//! **A mutation reddens one test or several, and which is worth knowing before you go hunting.**
//! On the tree as it stands: **1, 2, 4, 3** across the four conditions in order. Extra
//! reds naming the same condition are the expected shape, not a broken control.
//!
//! The rule behind those numbers is that every control here opens by asserting the real tree is
//! clean, so a mutation falsifies the opening of each control that reads the document it touched.
//! Condition 1 reddens only its own check, because the declaration control plants into string
//! literals and never reads `src/lib.rs`. Condition 2 reddens its check and its control. Condition
//! 3 reddens four — its check, its control,
//! [`the_dependency_scan_ignores_another_package_and_another_feature`] and
//! [`the_cross_package_walk_catches_a_planted_regression`], the last two because their plants also
//! start from a metadata document that now already carries an activation. Condition 4 reddens
//! three, the cross-package control included for the same reason.
//!
//! **These counts move whenever a control is added.** If you add a control that reads the real
//! metadata, re-measure rather than trusting this paragraph.
//!
//! Condition 4 is here because the sibling file measured it: three activations that reached
//! `dudes-rpc/testing` through a `default` table sat unmodelled in its scan and were confirmed real
//! with `cargo tree -e features -i` before being called holes. The same route was planted against
//! *this* scan before condition 4 existed — `default = ["dudes-supervisor/testing"]` in
//! `crates/dudes-host/Cargo.toml`, the crate `bins/duded` links from `[dependencies]` — and
//! [`shipping_activations`] reported nothing while `cargo tree -p duded -e features -i
//! dudes-supervisor` printed `dudes-supervisor feature "testing"` under `dudes-host feature
//! "default"`. A shipped `duded` linked the fakes and this file called the tree clean.
//!
//! Two more routes were found the same way *after* condition 4 was written, because the walk
//! followed only feature names local to one package and stopped at every `dep/feat`:
//! `harness = ["dudes-supervisor/testing"]` in `crates/dudes-host` with `features = ["harness"]` on
//! `bins/duded`'s shipping line, and `bins/duded` growing
//! `[features] default = ["dudes-host/harness"]`. Both left all ten tests green while a shipping
//! `duded` linked `FakeRng`. [`feature_reaches`] crosses packages for that reason and owns the
//! account.
//!
//! # Why conditions 2, 3 and 4 are read from `cargo metadata` and not from the TOML
//!
//! `dudes-rpc/tests/no_default_fakes.rs` is the precedent and it scans manifest **text**. That
//! choice cost it a TOML model — quote-aware bracket balancing, dotted headers, the
//! `[dependencies.<pkg>]` sub-table, `target.'cfg(unix)'.`, quoted keys, the `package = ` rename —
//! and its own header lists the spellings that still walk past it. Every one of those questions is
//! a question about *parsing*, and `cargo metadata --no-deps` is the answer Cargo itself computed:
//! a dependency arrives with its package name resolved through any rename, its `kind` (`null`,
//! `"dev"` or `"build"`), and the feature list it requests with `[workspace.dependencies]`
//! inheritance already merged in; a `[features]` table arrives as a map, whatever the TOML around
//! it looked like. That retires the *parsing* spellings, and it retires nothing else. **This scan
//! is not fail-closed and no comment here should say it is** — what is left is the residual list
//! below, which is what the next reader trusts instead of re-deriving the coverage, so it has to be
//! the whole of it.
//!
//! It is not a build. `--no-deps` resolves nothing, compiles nothing, and does not write
//! `Cargo.lock` — measured on this tree — so the objection the sibling file raises to running cargo
//! inside a test (a second target directory, a lockfile, a network posture) does not apply.
//! `--offline` is passed anyway: with no resolution to do it changes nothing, and it turns a
//! sandbox with no network into a clean failure rather than a hang.
//!
//! Condition 1 stays textual because it is about source, not a manifest, and no tool has parsed it.
//!
//! # What it does not close
//!
//! - **Only this workspace is read.** A crate outside it may depend on `dudes-supervisor` with
//!   `features = ["testing"]` and nothing here sees it. The gate turns reaching `FakeRng` into a
//!   line in a manifest instead of a `use` statement; it does not make it impossible.
//! - **`fuzz/` is its own workspace**, so `cargo metadata` on this one does not list it.
//!   [`the_excluded_fuzz_workspace_does_not_link_this_crate`] covers that one manifest the blunt
//!   way — it must not name this package at all — rather than by growing a second scanner.
//! - **`cargo build --workspace` is not the only build.** `cargo test --workspace` needs
//!   dev-dependencies, so resolver 3 unifies the feature onto the `dudes-supervisor` library for
//!   that whole invocation. That is the point of a dev-dependency and not a hole; the claim is
//!   about what links into a shipped artefact.
//! - **`--features dudes-supervisor/testing` on a command line** is written in no manifest, so
//!   nothing that reads manifests can see it. A CI job or a script is where that would appear.
//! - **The walk stops at any package `--no-deps` did not list**, which is every package outside
//!   this workspace. A chain that leaves the workspace and comes back is not followed. Nothing can
//!   arrange that today — this crate is unpublished, so no registry crate can name it — and the day
//!   one is published that stops being true.
//! - **A dev-dependency line asking for a sibling's feature that routes here is not reported**, the
//!   same way a dev line asking for `testing` directly is not. That is the arrangement
//!   `bins/duded` and `bins/dudes` are meant to have, and the claim these checks make is about what
//!   links into a shipped artefact.
//! - **Renaming the feature or the module** escapes the needles here. That direction is safe — a
//!   rename fails these tests loudly rather than passing silently.
//!
//! What is *not* on this list any more, having been measured rather than assumed: a `package = "…"`
//! rename in a feature entry is followed, because [`dep_key`] joins the unresolved string Cargo
//! hands through to the dependency record Cargo did resolve.
//!
//! This file names the feature and the module by necessity and reads only `src/lib.rs`,
//! `fuzz/Cargo.toml` and `cargo metadata`, so it does not scan itself.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

/// This crate's package name, as `cargo metadata` reports a dependency on it.
const PACKAGE: &str = "dudes-supervisor";

/// The feature that turns the fakes on.
const FEATURE: &str = "testing";

/// The declaration in `src/lib.rs` that the gate has to sit on.
const DECLARATION: &str = "pub mod testing;";

/// The attribute that must immediately precede it.
///
/// `cfg(test)` is not decoration: `src/token.rs`'s own `#[cfg(test)]` module reaches the fakes
/// through `crate::testing::FakeRng`, so a bare `#[cfg(feature = "testing")]` does not compile. The
/// exact text is pinned rather than parsed — a looser gate is a different rule and should be a diff
/// someone reads, not something a regex waves through.
const GATE: &str = "#[cfg(any(test, feature = \"testing\"))]";

/// The number of workspace packages [`metadata`] must report.
///
/// Ten crates, three binaries and `xtask`.
///
/// **The exact count, not a slack floor below it.** A value of 9 would be the one value
/// that makes its own purpose vacuous: `members = ["crates/*", "xtask"]` in the root manifest
/// yields exactly 9 packages and satisfies it, having dropped `duded` and `dudes` — two of the
/// three packages in this workspace that link this crate at all. A floor is here so that a walk
/// which has stopped seeing packages fails rather than reading as clean, and a floor with slack in
/// it tolerates losing precisely the packages that matter. A new member is one line of churn here
/// and a diff someone reads.
const MIN_PACKAGES: usize = 15;

/// The source at `path`, with CRLF line endings normalised to LF.
///
/// **Defence in depth, not a fix for a live bug.** A stray `\r` does not make
/// [`declares_ungated`] match or miss on line endings alone. The needle looks at the text *before* a declaration and
/// reaches it through `trim_end()`, which trims `\r` along with the rest of the whitespace, so the answers are the
/// same on un-normalised input. What the normalisation buys is that every needle added later gets
/// one kind of line ending to think about rather than two — which is worth keeping.
fn text_at(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .replace("\r\n", "\n")
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The workspace root: two levels above `crates/dudes-supervisor`.
fn workspace_root() -> PathBuf {
    let root = crate_root().join("..").join("..");
    assert!(
        root.join("Cargo.toml").is_file(),
        "no workspace manifest above {}; this scan reads the checkout's layout",
        crate_root().display()
    );
    root
}

// ---------------------------------------------------------------------------------------------
// 1. The declaration in `src/lib.rs`
// ---------------------------------------------------------------------------------------------

/// How many times `text` declares the module **without** a live [`GATE`] immediately before it.
///
/// The attribute is required to be adjacent, ignoring only whitespace: an attribute belongs to the
/// item it precedes, and anything between the two that is not whitespace means the reader is
/// looking at a different arrangement than this test is claiming. Comments therefore go *above* the
/// attribute, which is where `src/lib.rs` puts them.
fn declares_ungated(text: &str) -> usize {
    text.match_indices(DECLARATION)
        .filter(|(at, _)| !gated_at(text, *at))
        .count()
}

/// Whether the declaration starting at `at` is preceded by a [`GATE`] the compiler will read.
///
/// **"Live" is the whole of this function, and a plain `ends_with` does not have it.** Commenting
/// an attribute out is a more ordinary edit than deleting one — it is what anybody does to try
/// something — and `// #[cfg(any(test, feature = "testing"))]` ends with [`GATE`] exactly as the
/// attribute does. Without this arm, that one added character leaves `cargo build`
/// compiling the fakes into a default build, a scratch crate naming no feature printing tokens,
/// `clippy` silent and every other test here green.
///
/// So the match is confirmed twice: the text before the declaration ends with the attribute, and
/// the attribute is not inside a line comment. A block comment needs no arm — `/* … */` leaves
/// `*/` as the last thing before the declaration, which fails the first test already.
fn gated_at(text: &str, at: usize) -> bool {
    let before = text[..at].trim_end();
    if !before.ends_with(GATE) {
        return false;
    }
    let start = before.len() - GATE.len();
    let line_start = before[..start].rfind('\n').map_or(0, |newline| newline + 1);
    !before[line_start..start].contains("//")
}

/// How many times `text` declares the module at all, gated or not.
///
/// Counted so the check cannot pass by the declaration having been renamed out from under it: zero
/// declarations satisfies "none of them is ungated" perfectly.
fn declarations(text: &str) -> usize {
    text.matches(DECLARATION).count()
}

// ---------------------------------------------------------------------------------------------
// 2 and 3. What Cargo says about the workspace
// ---------------------------------------------------------------------------------------------

/// `cargo metadata --no-deps` over this workspace, parsed.
///
/// `env!("CARGO")` rather than the string `cargo`: it is the toolchain that is running this test,
/// which is the one whose reading of the manifests the rest of the build will use.
fn metadata() -> Value {
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--no-deps",
            "--format-version",
            "1",
            "--offline",
        ])
        .arg("--manifest-path")
        .arg(workspace_root().join("Cargo.toml"))
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cargo metadata emits JSON")
}

/// The packages `cargo metadata` listed.
fn packages(meta: &Value) -> &Vec<Value> {
    meta["packages"].as_array().expect("packages is an array")
}

/// The `[features]` table `package` declares, as Cargo parsed it.
///
/// `None` for a package that is not in the workspace. A package with no `[features]` table has an
/// empty map, which is a different answer and is load-bearing below: Cargo reports no implicit
/// `default` key, so a `default` entry in this map is one somebody wrote.
fn features_of<'a>(meta: &'a Value, package: &str) -> Option<&'a serde_json::Map<String, Value>> {
    packages(meta)
        .iter()
        .find(|p| p["name"] == package)?
        .get("features")?
        .as_object()
}

/// Whether `package` declares a feature named `key`.
fn declares_feature(meta: &Value, package: &str, key: &str) -> bool {
    features_of(meta, package).is_some_and(|table| table.contains_key(key))
}

/// `package`'s `default` feature entry, if it declared one.
fn default_entry(meta: &Value, package: &str) -> Option<Value> {
    features_of(meta, package)?.get("default").cloned()
}

/// The key a manifest writes for `dep`: the `package = "…"` rename if there is one, the package
/// name otherwise.
///
/// This is the string a feature entry's `key/feat` prefix has to match, and it is the reason a
/// rename no longer escapes the walk below: Cargo hands feature strings through unresolved, but it
/// resolves the dependency record, so the two can be joined here.
fn dep_key(dep: &Value) -> &str {
    dep["rename"]
        .as_str()
        .or_else(|| dep["name"].as_str())
        .unwrap_or_default()
}

/// The package `key` names in `from`'s dependency tables, whatever `from` called it.
///
/// Every table is searched, dev and build included: a feature entry may name a dev-dependency, and
/// whether *that* is a shipping route is the caller's question, not this one's.
fn dep_target<'a>(meta: &'a Value, from: &str, key: &str) -> Option<&'a str> {
    packages(meta)
        .iter()
        .find(|package| package["name"] == from)?["dependencies"]
        .as_array()?
        .iter()
        .find(|dep| dep_key(dep) == key)
        .and_then(|dep| dep["name"].as_str())
}

/// Whether `feature` of workspace package `package` reaches [`PACKAGE`]'s [`FEATURE`].
///
/// **The walk crosses packages, and it did not always.** It once queued an entry only when the
/// entry held no `/`, which followed *local* feature names and stopped at every `dep/feat`. Two
/// routes measured on this tree walked straight past it, each confirmed with
/// `cargo tree -e features,normal` and the lib unit's fingerprint before the scan was consulted,
/// and each leaving all ten tests here green while a shipping `duded` linked `FakeRng`:
///
/// - `harness = ["dudes-supervisor/testing"]` in `crates/dudes-host`, with `features = ["harness"]`
///   on `bins/duded`'s **shipping** dependency line — no `default`, no command line;
/// - `bins/duded` growing `[features] default = ["dudes-host/harness"]`, which is inside
///   [`no_default_feature_route_reaches_the_fakes`]'s own advertised scope.
///
/// Neither is exotic: "put the fixtures behind a feature and let the harness ask for it" is the
/// ordinary way to arrange this, and it is one hop.
///
/// So an entry is followed three ways. A bare name is a feature of `package` itself. A `dep:`
/// prefix is a dependency activation, not a feature, and is skipped. A `key/feat` is resolved —
/// `key` through [`dep_key`], so a `package = "…"` rename is followed rather than escaping — and if
/// the target is [`PACKAGE`] and `feat` is [`FEATURE`] the route is found; otherwise the walk
/// continues in the target's own table. The weak form `key?/feat` is followed like the plain one:
/// it turns the feature on whenever the optional dependency is enabled anyway, which is still a
/// route, and a manifest that grows one should be a diff someone reads.
///
/// `seen` makes a feature cycle terminate; a package outside this workspace has no table here, so
/// the walk stops there of its own accord.
fn feature_reaches(
    meta: &Value,
    package: &str,
    feature: &str,
    seen: &mut BTreeSet<(String, String)>,
) -> bool {
    if !seen.insert((package.to_owned(), feature.to_owned())) {
        return false;
    }
    let Some(entries) = features_of(meta, package)
        .and_then(|table| table.get(feature))
        .and_then(Value::as_array)
    else {
        return false;
    };
    for entry in entries.iter().filter_map(Value::as_str) {
        let reached = match entry.split_once('/') {
            Some((key, feat)) => {
                let key = key.strip_suffix('?').unwrap_or(key);
                match dep_target(meta, package, key) {
                    Some(PACKAGE) if feat == FEATURE => true,
                    Some(target) => feature_reaches(meta, target, feat, seen),
                    None => false,
                }
            }
            None if entry.starts_with("dep:") => false,
            None => feature_reaches(meta, package, entry, seen),
        };
        if reached {
            return true;
        }
    }
    false
}

/// Whether any feature in `features` of workspace package `package` reaches the fakes.
fn any_reaches(meta: &Value, package: &str, features: &[&str]) -> bool {
    features
        .iter()
        .any(|feature| feature_reaches(meta, package, feature, &mut BTreeSet::new()))
}

/// The features a dependency record asks for.
fn requested(dep: &Value) -> Vec<&str> {
    dep["features"]
        .as_array()
        .map(|features| features.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// Every dependency in this workspace that a shipped build reads and that turns [`FEATURE`] on,
/// directly or through the linked package's own feature table.
///
/// Three fields decide the edge, and each is one Cargo already resolved:
///
/// - `name` is the *package* linked, after any `package = "…"` rename, so the key someone wrote in
///   the manifest cannot disguise it;
/// - `kind` is `null` for a dependency a linked build reads, `"dev"` or `"build"` otherwise —
///   `"build"` is excluded with `"dev"` because a build script is not the shipped artefact;
/// - `features` is the requested list with `[workspace.dependencies]` inheritance merged in, so the
///   widest spelling of all (turning the feature on in the workspace table, where every
///   `workspace = true` inherits it) shows up on each inheriting member rather than being missed.
///
/// A `target.'cfg(…)'` table, a `[dependencies.<pkg>]` sub-table, a dotted preamble key and an
/// inline `features = { … }` are all the same record by the time they reach here.
///
/// What the record does *not* say is where a requested feature leads once it is inside the linked
/// package, so each one is handed to [`feature_reaches`]. A line asking for a feature of a sibling
/// that routes to the fakes is the same activation as asking for the fakes.
///
/// An edge that merely takes the target's defaults is not reported here: whether *those* reach the
/// fakes is a property of the target and [`default_routes`] reports it against the target, which is
/// the manifest that would have to change.
fn shipping_activations(meta: &Value) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for package in packages(meta) {
        let name = package["name"].as_str().unwrap_or_default();
        let Some(deps) = package["dependencies"].as_array() else {
            continue;
        };
        for dep in deps {
            let kind = dep["kind"].as_str();
            if kind == Some("dev") || kind == Some("build") {
                continue;
            }
            let Some(target) = dep["name"].as_str() else {
                continue;
            };
            let asks = requested(dep);
            let reaches = if target == PACKAGE {
                asks.contains(&FEATURE)
            } else {
                any_reaches(meta, target, &asks)
            };
            if reaches {
                found.insert(format!("{name} -> {target} {{ features = {asks:?} }}"));
            }
        }
    }
    found
}

/// The workspace packages that link [`PACKAGE`] from a table a shipped build reads.
///
/// The same `kind` rule as [`shipping_activations`], without the feature test: this is the set a
/// `default` route has somewhere to land in, and the control below plants into it rather than into
/// a package this file names.
fn shipping_dependents(meta: &Value) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for package in packages(meta) {
        let Some(deps) = package["dependencies"].as_array() else {
            continue;
        };
        for dep in deps {
            let kind = dep["kind"].as_str();
            if dep["name"] == PACKAGE && kind != Some("dev") && kind != Some("build") {
                found.insert(package["name"].as_str().unwrap_or_default().to_owned());
            }
        }
    }
    found
}

/// Every workspace package whose own `default` feature reaches the fakes.
///
/// This is the route a dependency record cannot carry, and it is not hypothetical: measured on this
/// tree, a `default = ["dudes-supervisor/testing"]` in `crates/dudes-host/Cargo.toml` left
/// [`shipping_activations`] reporting nothing while `cargo tree -p duded -e features -i
/// dudes-supervisor` showed `dudes-supervisor feature "testing"` under `dudes-host feature
/// "default"` — a shipped `duded` linking the fakes. A record carries the features that *dependency
/// line* asked for; a `[features]` table is a separate document, and `dep/feat` in it is a second
/// way to ask.
///
/// `default` is where the walk starts because it is the feature nobody has to ask for.
/// [`feature_reaches`] owns everything after that, the cross-package hops included.
///
/// It does not ask whether the reporting package's link to [`PACKAGE`] is a dev-dependency. Nothing
/// here pairs a `default` route with a dev-only link, and a package that grew one is reported
/// rather than passed — the blunter answer, for the reason the `fuzz/` check is blunt.
fn default_routes(meta: &Value) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for package in packages(meta) {
        let name = package["name"].as_str().unwrap_or_default();
        if feature_reaches(meta, name, "default", &mut BTreeSet::new()) {
            found.insert(format!("{name} -> `default` reaches {PACKAGE}/{FEATURE}"));
        }
    }
    found
}
// ---------------------------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------------------------

/// `src/lib.rs` declares the module once, behind the gate.
#[test]
fn the_module_is_declared_behind_the_gate() {
    let lib = text_at(&crate_root().join("src").join("lib.rs"));
    assert_eq!(
        declarations(&lib),
        1,
        "src/lib.rs must declare `{DECLARATION}` exactly once; a renamed declaration would make \
         the ungated count below vacuously zero"
    );
    assert_eq!(
        declares_ungated(&lib),
        0,
        "src/lib.rs declares `{DECLARATION}` without a live `{GATE}` immediately before it, so a \
         default build of this crate contains FakeRng and the counted agent token it mints — the \
         token half of invariant 8, which is what separates one agent from another on the socket"
    );
}

/// The feature exists and nothing turns it on by default.
#[test]
fn the_feature_is_off_by_default() {
    let meta = metadata();
    assert!(
        declares_feature(&meta, PACKAGE, FEATURE),
        "Cargo.toml declares no `{FEATURE}` feature; the gate in src/lib.rs then never opens and \
         the test suite cannot compile"
    );
    assert!(
        default_entry(&meta, PACKAGE).is_none(),
        "Cargo.toml's [features] table has a `default` entry ({:?}); this crate is meant to have \
         none, so that \"off unless asked for\" is a property of the table's shape",
        default_entry(&meta, PACKAGE)
    );
}

/// [`MIN_PACKAGES`] packages were listed, or the walk below is reading a workspace that is not this
/// one and every clean answer it gives is worthless.
///
/// **Called first, by both metadata checks,** before any offenders assertion, so that a real regression cannot
/// short-circuit the test before it runs — a floor that only reports on the runs that had nothing to report would be no floor.
fn assert_the_walk_saw_the_workspace(meta: &Value) {
    assert_eq!(
        packages(meta).len(),
        MIN_PACKAGES,
        "cargo metadata listed {} workspace packages, not {MIN_PACKAGES}; every \"nothing found\" \
         below is about whatever workspace this is, so fix the count here deliberately rather than \
         letting a walk that has stopped seeing members read as clean",
        packages(meta).len()
    );
}

/// No build in this workspace turns the feature on outside a dev-dependency.
#[test]
fn no_shipping_dependency_asks_for_the_fakes() {
    let meta = metadata();
    assert_the_walk_saw_the_workspace(&meta);
    let offenders = shipping_activations(&meta);
    assert!(
        offenders.is_empty(),
        "these packages link {PACKAGE} with `{FEATURE}` reachable from a table a shipped build \
         reads, which puts FakeRng's counted agent token — the token half of invariant 8 — in a \
         shipped build; a test that needs the fakes asks for them from [dev-dependencies]: \
         {offenders:#?}"
    );
}

/// No package's own `default` feature reaches the fakes.
///
/// The second way to ask, and the one a dependency line does not record. See [`default_routes`] for
/// the measurement that put this check here.
#[test]
fn no_default_feature_route_reaches_the_fakes() {
    let meta = metadata();
    assert_the_walk_saw_the_workspace(&meta);
    let routes = default_routes(&meta);
    assert!(
        routes.is_empty(),
        "these packages reach `{PACKAGE}/{FEATURE}` from their own `default`, which turns the \
         feature on for every build of them and of anything that links them, and no dependency \
         line records it — a guessable agent token, which is the token half of invariant 8, in \
         every shipped build downstream: {routes:#?}"
    );
}

/// The one manifest `cargo metadata` on this workspace cannot see.
///
/// `fuzz/` is excluded from the root workspace (nightly, and a libFuzzer licence the allow list
/// refuses), so it is its own workspace and no dependency of it appears above. It links
/// `dudes-journal` and nothing else today; the check is the blunt one — it must not name this
/// package at all — because a second TOML scanner to answer a subtler question about one file is
/// the duplication this file exists to avoid.
#[test]
fn the_excluded_fuzz_workspace_does_not_link_this_crate() {
    let manifest = workspace_root().join("fuzz").join("Cargo.toml");
    if !manifest.is_file() {
        return;
    }
    let text = text_at(&manifest);
    assert!(
        text.contains("dudes-journal"),
        "positive control: fuzz/Cargo.toml names the crate it does depend on, so a miss below is \
         an absence and not an unread file"
    );
    assert!(
        !text.contains(PACKAGE),
        "fuzz/Cargo.toml names {PACKAGE}; that workspace is outside `cargo metadata` here, so a \
         dependency of it needs reading by a human rather than passing silently"
    );
}

/// The declaration scan can fail, in the shape it exists to catch.
///
/// One control per scan rather than one for all three: a single function covering them would say
/// only that *something* is unproven when it goes red, and this is the file whose whole job is
/// telling one silent regression from another.
#[test]
fn the_declaration_scan_catches_a_planted_regression() {
    let gated = format!("pub mod world;\n// a comment\n{GATE}\n{DECLARATION}\n");
    assert_eq!(
        declarations(&gated),
        1,
        "positive control: the gated form is a declaration"
    );
    assert_eq!(
        declares_ungated(&gated),
        0,
        "the real arrangement — a comment above the attribute, the attribute against the item — \
         must stay legal"
    );
    assert_eq!(
        declares_ungated(&format!("pub mod world;\n{DECLARATION}\n")),
        1,
        "a declaration with no attribute at all is the regression"
    );
    assert_eq!(
        declares_ungated(&format!("{GATE}\n// a comment\n{DECLARATION}\n")),
        1,
        "a comment between the attribute and the item is a different arrangement, and this test \
         claims the adjacent one"
    );
    assert_eq!(
        declares_ungated(&format!("#[cfg(feature = \"{FEATURE}\")]\n{DECLARATION}\n")),
        1,
        "the bare feature gate is caught: it is the one that does not compile, because \
         src/token.rs's unit tests reach the fakes through `crate::testing`"
    );
    assert_eq!(
        declares_ungated(&format!("// {GATE}\n{DECLARATION}\n")),
        1,
        "a commented-out attribute is not a gate: without this arm that one \
         character ships FakeRng in a default build with clippy silent and this file green — \
         and commenting an attribute out to try something is a more ordinary edit than deleting it"
    );
    assert_eq!(
        declares_ungated(&format!("//{GATE}\n{DECLARATION}\n")),
        1,
        "with no space either"
    );
    assert_eq!(
        declares_ungated(&format!("/// {GATE}\n{DECLARATION}\n")),
        1,
        "a doc comment that quotes the attribute is not the attribute"
    );
    assert_eq!(
        declares_ungated(&format!("/* {GATE} */\n{DECLARATION}\n")),
        1,
        "a block-commented attribute leaves `*/` before the declaration, which is not the gate"
    );
    assert_eq!(
        declares_ungated(&format!("// a comment\n{GATE}\n{DECLARATION}\n")),
        0,
        "an attribute on its own line below a comment is the real arrangement and stays legal"
    );
    assert_eq!(
        declarations("pub mod world;\npub mod machine;\n"),
        0,
        "a tree with no declaration is reported as such rather than passing the ungated count"
    );
}

/// The `[features]` check can fail, in the shape it exists to catch.
///
/// Planted into the **real** metadata document rather than into a literal: the thing under test is
/// how this file reads what Cargo emits, and a hand-written stand-in for that document is a second
/// claim about its shape.
#[test]
fn the_feature_scan_catches_a_planted_regression() {
    let mut meta = metadata();
    assert!(
        declares_feature(&meta, PACKAGE, FEATURE),
        "positive control: the feature is declared on the tree as it stands"
    );
    assert!(
        default_entry(&meta, PACKAGE).is_none(),
        "positive control: no `default` entry is the shape this crate ships"
    );

    let package = packages_mut(&mut meta)
        .iter_mut()
        .find(|p| p["name"] == PACKAGE)
        .expect("this crate is in its own workspace metadata");
    package["features"]["default"] = Value::Array(vec![Value::String(FEATURE.to_owned())]);
    assert_eq!(
        default_entry(&meta, PACKAGE),
        Some(Value::Array(vec![Value::String(FEATURE.to_owned())])),
        "a `default` table naming the feature is the regression, and the check reads it"
    );

    assert!(
        !declares_feature(&meta, PACKAGE, "no-such-feature"),
        "a feature nobody declared is not declared"
    );
    assert!(
        !declares_feature(&meta, "no-such-package", FEATURE),
        "a package that is not in the workspace declares nothing here"
    );
}

/// The dependency check can fail, in the shape it exists to catch: a dev-dependency that has become
/// a shipping one.
///
/// Planted into the real metadata for the reason above — and this plant is exactly the edit it
/// guards against, moving `bins/duded`'s `[dev-dependencies]` line up into `[dependencies]`, which
/// is the one that would put the counted mint one `use` statement from the daemon's `main.rs`.
#[test]
fn the_dependency_scan_catches_a_planted_regression() {
    let mut meta = metadata();
    assert!(
        shipping_activations(&meta).is_empty(),
        "positive control: the tree as it stands has no shipping activation"
    );

    let mut planted = 0;
    for package in packages_mut(&mut meta) {
        let Some(deps) = package["dependencies"].as_array_mut() else {
            continue;
        };
        for dep in deps {
            if dep["name"] == PACKAGE && dep["kind"] == "dev" {
                dep["kind"] = Value::Null;
                planted += 1;
            }
        }
    }
    assert!(
        planted > 0,
        "positive control: at least one dev-dependency on {PACKAGE} exists to promote, or this \
         plant proves nothing"
    );
    assert_eq!(
        shipping_activations(&meta).len(),
        planted,
        "every dev-dependency promoted to a shipping one is reported"
    );

    // A build script is not the shipped artefact, and must not be reported.
    for package in packages_mut(&mut meta) {
        let Some(deps) = package["dependencies"].as_array_mut() else {
            continue;
        };
        for dep in deps {
            if dep["name"] == PACKAGE {
                dep["kind"] = Value::String("build".to_owned());
            }
        }
    }
    assert!(
        shipping_activations(&meta).is_empty(),
        "a build-dependency is not a shipping one"
    );
}

/// The dependency check is about *this* package and *this* feature, and neither neighbour.
#[test]
fn the_dependency_scan_ignores_another_package_and_another_feature() {
    let mut meta = metadata();
    for package in packages_mut(&mut meta) {
        let Some(deps) = package["dependencies"].as_array_mut() else {
            continue;
        };
        for dep in deps {
            if dep["name"] == PACKAGE && dep["kind"] == "dev" {
                dep["kind"] = Value::Null;
                dep["name"] = Value::String("some-other-crate".to_owned());
            }
        }
    }
    assert!(
        shipping_activations(&meta).is_empty(),
        "another package's `{FEATURE}` feature is not this one's"
    );

    let mut meta = metadata();
    for package in packages_mut(&mut meta) {
        let Some(deps) = package["dependencies"].as_array_mut() else {
            continue;
        };
        for dep in deps {
            if dep["name"] == PACKAGE && dep["kind"] == "dev" {
                dep["kind"] = Value::Null;
                dep["features"] =
                    Value::Array(vec![Value::String("some-other-feature".to_owned())]);
            }
        }
    }
    assert!(
        shipping_activations(&meta).is_empty(),
        "a shipping dependency that asks for a different feature is not this regression"
    );
}

/// The `default`-route scan can fail, in the three shapes it exists to catch.
///
/// Planted into the real metadata, into whichever packages actually link this crate from a shipping
/// table — the plant is then the edit that would really do it, not a package name this file has
/// hard-coded and that a later split could make wrong.
#[test]
fn the_default_route_scan_catches_a_planted_regression() {
    let meta = metadata();
    assert!(
        default_routes(&meta).is_empty(),
        "positive control: the tree as it stands has no `default` route"
    );
    let dependents = shipping_dependents(&meta);
    assert!(
        !dependents.is_empty(),
        "positive control: some package links {PACKAGE} from a shipping table, or these plants \
         prove nothing"
    );

    // 1. The direct spelling — `default = ["dudes-supervisor/testing"]`.
    let direct = plant_default(&meta, &dependents, |_| {
        vec![Value::String(format!("{PACKAGE}/{FEATURE}"))]
    });
    assert_eq!(
        default_routes(&direct).len(),
        dependents.len(),
        "a `default` naming the route directly is the regression, and every package with one is \
         reported"
    );

    // 2. One named hop — `default = ["harness"]`, `harness = ["dudes-supervisor/testing"]`.
    let mut hopped = meta.clone();
    for package in packages_mut(&mut hopped) {
        if !dependents.contains(package["name"].as_str().unwrap_or_default()) {
            continue;
        }
        package["features"]["default"] = Value::Array(vec![Value::String("harness".to_owned())]);
        package["features"]["harness"] =
            Value::Array(vec![Value::String(format!("{PACKAGE}/{FEATURE}"))]);
    }
    assert_eq!(
        default_routes(&hopped).len(),
        dependents.len(),
        "a `default` that names a local feature that names the route is the same regression one \
         line further away"
    );

    // 3. The weak form — `dudes-supervisor?/testing`.
    let weak = plant_default(&meta, &dependents, |_| {
        vec![Value::String(format!("{PACKAGE}?/{FEATURE}"))]
    });
    assert_eq!(
        default_routes(&weak).len(),
        dependents.len(),
        "the weak spelling is a route too and is reported"
    );

    // Neither neighbour: another package's feature, and another feature of this one.
    let other_package = plant_default(&meta, &dependents, |_| {
        vec![Value::String(format!("some-other-crate/{FEATURE}"))]
    });
    assert!(
        default_routes(&other_package).is_empty(),
        "another package's `{FEATURE}` feature is not this one's"
    );
    let other_feature = plant_default(&meta, &dependents, |_| {
        vec![Value::String(format!("{PACKAGE}/some-other-feature"))]
    });
    assert!(
        default_routes(&other_feature).is_empty(),
        "another feature of this package is not this regression"
    );

    // A cycle in the feature table terminates rather than hanging.
    let mut cyclic = meta.clone();
    for package in packages_mut(&mut cyclic) {
        if !dependents.contains(package["name"].as_str().unwrap_or_default()) {
            continue;
        }
        package["features"]["default"] = Value::Array(vec![Value::String("a".to_owned())]);
        package["features"]["a"] = Value::Array(vec![Value::String("b".to_owned())]);
        package["features"]["b"] = Value::Array(vec![Value::String("a".to_owned())]);
    }
    assert!(
        default_routes(&cyclic).is_empty(),
        "a feature cycle that reaches nothing reaches nothing"
    );
}

/// A `(dependent, middle)` pair where `dependent` links `middle` from a table a shipped build
/// reads, and `middle` links [`PACKAGE`] from one.
///
/// The shape both cross-package routes need, found in the tree rather than named here: today it is
/// `duded -> dudes-host -> dudes-supervisor`.
fn a_two_hop_chain(meta: &Value) -> Option<(String, String)> {
    let middles = shipping_dependents(meta);
    for package in packages(meta) {
        let name = package["name"].as_str().unwrap_or_default();
        let Some(deps) = package["dependencies"].as_array() else {
            continue;
        };
        for dep in deps {
            let kind = dep["kind"].as_str();
            if kind == Some("dev") || kind == Some("build") {
                continue;
            }
            let target = dep["name"].as_str().unwrap_or_default();
            if target != PACKAGE && middles.contains(target) {
                return Some((name.to_owned(), target.to_owned()));
            }
        }
    }
    None
}

/// `meta` with `middle` grown a `harness` feature that reaches the fakes.
fn plant_harness(meta: &Value, middle: &str) -> Value {
    let mut planted = meta.clone();
    for package in packages_mut(&mut planted) {
        if package["name"] == middle {
            package["features"]["harness"] =
                Value::Array(vec![Value::String(format!("{PACKAGE}/{FEATURE}"))]);
        }
    }
    planted
}

/// The walk crosses packages, in the three shapes that once walked past it.
///
/// Its own control rather than an arm inside the two scan controls, because [`feature_reaches`] is
/// the one function both scans share: a red here says the *walk* stopped crossing, which is a
/// different diagnosis from either scan having lost its own condition.
///
/// Each plant is one that was first measured against the **real** manifests — cargo confirming the
/// feature was genuinely on before the scan was consulted, with `cargo tree -e features,normal` and
/// the `dudes-supervisor` lib unit's fingerprint after a fresh `cargo build -p duded`. All three
/// left every test in this file green.
#[test]
fn the_cross_package_walk_catches_a_planted_regression() {
    let meta = metadata();
    let (dependent, middle) = a_two_hop_chain(&meta).expect(
        "positive control: some package links a sibling that links this crate, or these plants \
         prove nothing",
    );
    assert!(
        shipping_activations(&meta).is_empty() && default_routes(&meta).is_empty(),
        "positive control: the tree as it stands has neither route"
    );

    // 1. The sibling grows the feature and the shipping dependency line asks for it. No `default`,
    //    no command line: `harness = ["dudes-supervisor/testing"]` plus `features = ["harness"]`.
    let mut asked = plant_harness(&meta, &middle);
    for package in packages_mut(&mut asked) {
        if package["name"] != dependent.as_str() {
            continue;
        }
        for dep in package["dependencies"].as_array_mut().into_iter().flatten() {
            if dep["name"] == middle.as_str() {
                dep["features"] = Value::Array(vec![Value::String("harness".to_owned())]);
            }
        }
    }
    assert!(
        !shipping_activations(&asked).is_empty(),
        "a shipping line asking for a sibling feature that routes to `{FEATURE}` is the same \
         activation as asking for `{FEATURE}`, and {dependent} -> {middle} must be reported"
    );

    // 2. The same sibling feature, reached from the dependent's own `default` instead.
    let mut routed = plant_harness(&meta, &middle);
    for package in packages_mut(&mut routed) {
        if package["name"] == dependent.as_str() {
            package["features"]["default"] =
                Value::Array(vec![Value::String(format!("{middle}/harness"))]);
        }
    }
    assert!(
        default_routes(&routed).contains(&format!(
            "{dependent} -> `default` reaches {PACKAGE}/{FEATURE}"
        )),
        "a `default` that names a *sibling's* feature is inside this check's advertised scope and \
         has to be walked"
    );

    // 3. The same route with the sibling renamed, which is what `dep_key` is for.
    let mut renamed = plant_harness(&meta, &middle);
    for package in packages_mut(&mut renamed) {
        if package["name"] != dependent.as_str() {
            continue;
        }
        for dep in package["dependencies"].as_array_mut().into_iter().flatten() {
            if dep["name"] == middle.as_str() {
                dep["rename"] = Value::String("midalias".to_owned());
            }
        }
        package["features"]["default"] =
            Value::Array(vec![Value::String("midalias/harness".to_owned())]);
    }
    assert!(
        !default_routes(&renamed).is_empty(),
        "a `package = \"…\"` rename changes the key a feature entry writes and nothing else; the \
         walk resolves it through the dependency record"
    );

    // And the neighbours: a key naming nothing, and the right key with the wrong feature.
    let mut stranger = plant_harness(&meta, &middle);
    for package in packages_mut(&mut stranger) {
        if package["name"] == dependent.as_str() {
            package["features"]["default"] =
                Value::Array(vec![Value::String("no-such-dependency/harness".to_owned())]);
        }
    }
    assert!(
        default_routes(&stranger).is_empty(),
        "a key that resolves to no dependency of this package reaches nothing"
    );
    let mut wrong_feature = plant_harness(&meta, &middle);
    for package in packages_mut(&mut wrong_feature) {
        if package["name"] == dependent.as_str() {
            package["features"]["default"] =
                Value::Array(vec![Value::String(format!("{middle}/some-other-feature"))]);
        }
    }
    assert!(
        default_routes(&wrong_feature).is_empty(),
        "the sibling's other features are not this one"
    );

    // A cycle in the sibling's own table, reached across the package boundary, terminates.
    let mut cyclic = meta.clone();
    for package in packages_mut(&mut cyclic) {
        if package["name"] == dependent.as_str() {
            package["features"]["default"] =
                Value::Array(vec![Value::String(format!("{middle}/loop"))]);
        }
        if package["name"] == middle.as_str() {
            package["features"]["loop"] = Value::Array(vec![Value::String("knot".to_owned())]);
            package["features"]["knot"] = Value::Array(vec![Value::String("loop".to_owned())]);
        }
    }
    assert!(
        default_routes(&cyclic).is_empty(),
        "a feature cycle reached across a package boundary ends rather than hanging"
    );
}

/// `meta` with `entries(name)` written as the `default` feature of every package in `into`.
fn plant_default(
    meta: &Value,
    into: &BTreeSet<String>,
    entries: impl Fn(&str) -> Vec<Value>,
) -> Value {
    let mut planted = meta.clone();
    for package in packages_mut(&mut planted) {
        let name = package["name"].as_str().unwrap_or_default().to_owned();
        if into.contains(&name) {
            package["features"]["default"] = Value::Array(entries(&name));
        }
    }
    planted
}

/// The packages array, mutably, for the plants above.
fn packages_mut(meta: &mut Value) -> &mut Vec<Value> {
    meta["packages"]
        .as_array_mut()
        .expect("packages is an array")
}
