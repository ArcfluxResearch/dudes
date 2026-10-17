//! the `unsafe` grant is two named modules, and its size is a **test**.
//!
//! The crate sets `unsafe_code = "deny"` rather than inheriting the workspace's `forbid`, because
//! `forbid` cannot be re-levelled from source and therefore admits no module-scoped exemption at
//! all. What that buys back is the ability to write `#[allow(unsafe_code)]` on exactly the named
//! modules — and what stops it becoming a habit is this file. A grant that spreads by one module at a time
//! ends up as a platform surface nobody reads; a grant whose growth is a red test does not.
//!
//! The scan reads code lines only, and that is the whole reason it works here: every module in this
//! crate explains at length why it does *not* contain `unsafe`, and a scan that matched prose would
//! either forbid those explanations or be turned off.
//!
//! # Exactly how far the scan reaches, so the claim is the size of its evidence
//!
//! It walks **`src/`**, so what it pins is a statement about `src/` and not about the crate. A file
//! under `tests/` can re-level the crate's `deny` from its own source — `#![allow(unsafe_code)]`
//! followed by an `unsafe` block compiles, and was checked — and this scan would not see it. That is
//! a real gap and it is stated rather than closed here: a test that reaches for `unsafe` is visible
//! in review in a way a platform module quietly growing one is not, and widening the walk is its own
//! change.
//!
//! It also matches by **file name**, not by path, so moving `src/windows.rs` to
//! `src/plat/windows.rs` leaves [`the_unsafe_grant_covers_the_named_modules_and_no_other`]
//! green — measured, not supposed. The move does not get through this file, but only because
//! [`the_windows_layer_never_claims_positive_absence`] reads a fixed path and goes red instead;
//! that is luck, not design, and the two would come apart the moment the second test changed.
//! Noted rather than fixed: widening the walk to match on path is its own change, and a hole
//! written down is not the same kind of problem as one nobody knows about.

mod common;

use std::fs;

/// The Windows module, which owns the process-ownership grant and is the one measured below.
const GRANTED: &str = "windows.rs";

/// The Unix module that clears the signal mask between `fork` and `exec`: one block, nothing else.
const SIGMASK: &str = "sigmask.rs";

/// Every file the grant covers, matched by name — see the module docs on what that does not
/// catch, in sorted order. Changing this line is the visible cost of widening the grant.
const GRANTED_FILES: &[&str] = &[SIGMASK, GRANTED];

/// The shapes `unsafe` takes in code. Deliberately not the bare word: the bare word appears all
/// over the prose of this crate, and matching it there would make the guard unusable.
const GRANTS: &[&str] = &[
    "unsafe_code",
    "unsafe {",
    "unsafe fn",
    "unsafe impl",
    "unsafe trait",
    "unsafe extern",
];

/// `unsafe` appears in the Windows platform module and the signal-mask module and in no other file
/// **under `src/`**. This is the test that goes red when the exemption spreads there; the module
/// docs say what it does not reach.
#[test]
fn the_unsafe_grant_covers_the_named_modules_and_no_other() {
    let sources = common::rust_sources();
    assert!(
        sources.len() >= 5,
        "scanned only {} files; is src/ where it was?",
        sources.len()
    );
    let mut granted_files = Vec::new();
    for path in &sources {
        let text = fs::read_to_string(path).expect("read a source file");
        let found = common::found_in_code(&text, GRANTS);
        if found.is_empty() {
            continue;
        }
        let name = path.file_name().expect("a file name").to_string_lossy();
        assert!(
            GRANTED_FILES.contains(&&*name),
            "{} contains {found:?}; the unsafe grant is {GRANTED_FILES:?} alone",
            path.display()
        );
        granted_files.push(name.into_owned());
    }
    // Not vacuous, and this is the deliberate strengthening: the scan reads files from disk rather
    // than compiling them, so `src/windows.rs` is scanned on every runner even where it is not
    // compiled. A grant added on a machine that never builds for Windows still goes red.
    granted_files.sort();
    assert_eq!(
        granted_files,
        GRANTED_FILES
            .iter()
            .map(|name| (*name).to_owned())
            .collect::<Vec<_>>(),
        "the granted modules should be the only files using the grant"
    );
}

/// The scan above passes by finding almost nothing, which proves nothing unless the scan can fail.
///
/// Two directions, and both matter. A planted `unsafe` in each of its code shapes must be caught,
/// or a typo in [`GRANTS`] sits green forever and the grant is unpinned while looking pinned. And
/// a source that only *talks* about `unsafe` must not be caught, or the guard's first false alarm
/// is the reason someone deletes it.
#[test]
fn the_scan_catches_a_planted_grant_and_spares_prose() {
    for (planted, expected) in [
        (
            "let h = unsafe { OpenProcess(ACCESS, 0, pid) };",
            "unsafe {",
        ),
        (
            "pub unsafe fn creation_time(handle: HANDLE) -> u64 { 0 }",
            "unsafe fn",
        ),
        ("#![allow(unsafe_code)]", "unsafe_code"),
        ("unsafe impl Send for Grip {}", "unsafe impl"),
        ("unsafe trait Reserved {}", "unsafe trait"),
        ("unsafe extern \"C\" { fn f(); }", "unsafe extern"),
    ] {
        // `contains(&expected)` rather than `!is_empty()`: with overlapping shapes another entry
        // could otherwise answer for this one, and the case would prove nothing about `expected`.
        assert!(
            common::found_in_code(planted, GRANTS).contains(&expected),
            "the scan did not catch `{expected}` in `{planted}`"
        );
    }

    let prose = "//! No `unsafe` in this file, and this test is what says so.\n\
                 /// An `unsafe fn` would be caught here if this were code.\n\
                 // SAFETY: not applicable — there is no unsafe { } block below.\n\
                 pub(crate) fn probe(pid: u32) -> u64 { 0 }\n";
    assert_eq!(
        common::found_in_code(prose, GRANTS),
        Vec::<&str>::new(),
        "a module that only discusses unsafe must not be flagged"
    );

    // A tripwire rather than a coverage claim: every shape above has a planted case, so adding one
    // is a prompt to add its case too.
    assert_eq!(
        GRANTS.len(),
        6,
        "the grant shapes changed; check whether the new entry needs a planted case above"
    );
}

/// on Windows, positive absence is unreachable, deliberately.
///
/// No Microsoft page documents which error `OpenProcess` returns for an ordinary nonexistent
/// identifier, so a failed open is `CouldNotAnswer` and never `NoSuchPid`. That narrowing is
/// sufficient for the kill path — `Unknown` is not `Alive`, so the kill refuses and a refusal
/// cannot signal a stranger — and insufficient for positive absence, which is separate work. Until
/// that work lands, this test is what keeps the narrowing from being quietly undone by someone
/// mapping an errno they read on a forum.
///
/// **The control is the point:** the same scan over the Linux layer, which *does* have positive
/// absence, finds the variant. An absence that a scan cannot demonstrate finding is worthless.
#[test]
fn the_windows_layer_never_claims_positive_absence() {
    let absence = &["NoSuchPid"];
    let windows = fs::read_to_string(common::src_dir().join(GRANTED)).expect("read windows.rs");
    assert_eq!(
        common::found_in_code(&windows, absence),
        Vec::<&str>::new(),
        "src/{GRANTED} returns NoSuchPid; every OpenProcess failure must be \
         CouldNotAnswer, because the error code for an absent identifier is not sourced"
    );

    let linux = fs::read_to_string(common::src_dir().join("linux.rs")).expect("read linux.rs");
    assert_eq!(
        common::found_in_code(&linux, absence),
        vec!["NoSuchPid"],
        "control: the Linux layer does answer NoSuchPid, so the absence above is a fact about \
         that module and not about this scan"
    );
}

/// The unsafe grant, measured rather than merely located.
///
/// One row per shape, and the number is the whole point. The test above pins *where* `unsafe` may
/// appear; nothing pinned *how much*, so the grant could have doubled inside `src/windows.rs` with
/// every check in this file green. A grant that grows a block at a time ends up as a platform
/// surface nobody reads, which is the failure the unsafe grant exists to prevent, and "one
/// module" stops being a meaningful bound once the module is large enough.
///
/// **A count, deliberately, and not a list of the functions called.** The unsafe grant is scoped
/// by module, not by an enumerated function list, because an enumeration is a second list to
/// erode. A total honours that: it names no Win32 call, needs no upkeep when one is swapped for
/// another, and still goes red when the quantity changes.
///
/// **Updating these numbers is the intended cost of adding `unsafe`, not an obstacle to it.** When
/// a row moves, the questions are the ones review would ask anyway: does every new block carry a
/// `// SAFETY:` comment naming what it relies on, and is the call one no crate exposes safely? Move
/// the number in the same change, and say in the commit why the grant grew.
const GRANT_SIZE: &[(&str, usize)] = &[
    // The module attribute itself. Exactly one, and a second would mean a second granted module.
    ("unsafe_code", 1),
    // Blocks. The number that measures the process-ownership grant plus the injected pipe halt,
    // and the peer walk: one `IsProcessInJob`, one `CreateToolhelp32Snapshot`, one
    // `Process32FirstW`, one `Process32NextW` and the snapshot's `CloseHandle`.
    ("unsafe {", 12),
    // No unsafe function remains: the DACL's raw ACL/SID walkers moved with that responsibility.
    ("unsafe fn", 0),
    // `unsafe impl`: `Send for Grip`, argued in the module at the impl.
    ("unsafe impl", 1),
    // Neither shape appears, and a zero row is not filler: it is what makes an `unsafe trait` or
    // an `unsafe extern` block arriving here a red test rather than an unremarked addition.
    ("unsafe trait", 0),
    ("unsafe extern", 0),
];

/// The grant is exactly this large, shape by shape.
///
/// Red in both directions on purpose. Growth is the case this exists for; a *fall* is worth
/// catching too, because the usual way a count falls is that a block was deleted along with the
/// call it guarded, and this file is then reporting on a module that no longer does what its rows
/// describe.
#[test]
fn the_grant_is_exactly_the_size_recorded_here() {
    let windows = fs::read_to_string(common::src_dir().join(GRANTED)).expect("read windows.rs");
    let measured: Vec<(&str, usize)> = GRANT_SIZE
        .iter()
        .map(|(shape, _)| (*shape, common::count_in_code(&windows, shape)))
        .collect();
    assert_eq!(
        measured,
        GRANT_SIZE.to_vec(),
        "the size of the `unsafe` grant in src/{GRANTED} changed. This is a real \
         review prompt, not a chore: check that every block carries a `// SAFETY:` comment naming \
         what it relies on and that the call is one no crate exposes safely, then move the row in \
         the same change and say in the commit why the grant grew"
    );

    // Every shape this file knows how to find is measured, row for row and in the same order, so
    // a new entry in `GRANTS` cannot be added without a size row — which is how the two tables
    // stay the same table. Names rather than lengths: a table of the right length with one shape
    // listed twice and another not at all has the same length, and no row for the missing shape.
    let sized: Vec<&str> = GRANT_SIZE.iter().map(|(shape, _)| *shape).collect();
    assert_eq!(
        sized, GRANTS,
        "GRANT_SIZE must hold exactly the shapes in GRANTS, once each and in that order; a shape \
         with no row is one the grant can grow in unmeasured"
    );
}

/// The signal-mask module holds exactly one block and one module attribute, and nothing else.
#[test]
fn the_signal_mask_grant_is_one_block() {
    let text = fs::read_to_string(common::src_dir().join(SIGMASK)).expect("read sigmask.rs");
    let measured: Vec<(&str, usize)> = GRANTS
        .iter()
        .map(|shape| (*shape, common::count_in_code(&text, shape)))
        .collect();
    assert_eq!(
        measured,
        vec![
            ("unsafe_code", 1),
            ("unsafe {", 1),
            ("unsafe fn", 0),
            ("unsafe impl", 0),
            ("unsafe trait", 0),
            ("unsafe extern", 0),
        ],
        "the grant in src/{SIGMASK} changed; it is meant to hold one `pre_exec` block"
    );
}

/// The control: the counter counts, and counts code rather than prose.
///
/// Without it a `count_in_code` that returned 0 for everything would make the table above a list of
/// zeros nobody had noticed, and a `count_in_code` that read comments would count the paragraphs
/// this crate uses to explain why its modules hold no `unsafe`.
#[test]
fn the_counter_counts_code_and_not_prose() {
    let sample = "let a = unsafe { one() };\nlet b = unsafe { two() };\nfn plain() {}\n";
    assert_eq!(
        common::count_in_code(sample, "unsafe {"),
        2,
        "control: two blocks on two code lines must count as two"
    );

    // Occurrences, not lines. A counter that counted the lines *containing* the token agrees with
    // the sample above, where every block has a line to itself, and disagrees only here.
    let doubled = "let (a, b) = (unsafe { one() }, unsafe { two() });\n";
    assert_eq!(
        common::count_in_code(doubled, "unsafe {"),
        2,
        "control: two blocks on one code line must count as two, or the counter counts lines"
    );

    let prose = "// This module has no unsafe { } block, and says so twice.\n\
                 /// Nor an `unsafe fn`, which this line only mentions.\n";
    assert_eq!(
        common::count_in_code(prose, "unsafe {"),
        0,
        "control: a comment describing a block must not be counted as one"
    );

    // The direction that matters most: a planted block must move the total, or the equality above
    // is satisfied by a counter that ignores additions.
    let windows = fs::read_to_string(common::src_dir().join(GRANTED)).expect("read windows.rs");
    let planted = format!("{windows}\nfn extra() {{ let _ = unsafe {{ probe() }}; }}\n");
    assert_eq!(
        common::count_in_code(&planted, "unsafe {"),
        common::count_in_code(&windows, "unsafe {") + 1,
        "control: one planted block must raise the measured total by exactly one"
    );
}
