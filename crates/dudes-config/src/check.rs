//! The completion check and the defaults a new project gets, shared by `dudes project add` and the
//! Floor's add.

use std::io::Read as _;
use std::path::Path;

/// The department an enrolled workspace and a first hire get unless the operator names one.
pub const DEFAULT_DEPARTMENT: &str = "engineering";
/// The most turns an agent takes on one goal unless the operator says otherwise.
pub const DEFAULT_HOP_CAP: u32 = 5;
/// Seconds allowed for a suggested completion check.
pub const DEFAULT_PREDICATE_TIMEOUT_S: u64 = 600;

/// The most bytes read from any one file of a repository while looking for its test command.
const MAX_READ_BYTES: u64 = 1 << 20;

/// The label the Floor shows for [`changed_check`].
const CHANGED_LABEL: &str = "files changed";

/// Said wherever [`changed_check`] is offered, so the default is never a surprise.
pub const NO_TEST_CHECK: &str = "No test command was found here. The default check passes once the agent has edited or added a \
     file that is not committed yet. Dudes runs it itself, then you review the change.";

/// The check a repository suggests for itself: `None` when nothing in it names one.
#[must_use]
pub fn suggest_check(dir: &Path) -> Option<Vec<String>> {
    let read = |name: &str| {
        let path = dir.join(name);
        // A link, a FIFO or a device is not read: opening one can block this thread for good.
        if !std::fs::symlink_metadata(&path).ok()?.file_type().is_file() {
            return None;
        }
        let mut text = String::new();
        std::fs::File::open(path)
            .ok()?
            .take(MAX_READ_BYTES)
            .read_to_string(&mut text)
            .ok()?;
        Some(text)
    };
    let argv: &[&str] = if dir.join("Cargo.toml").is_file() {
        &["cargo", "test"]
    } else if read("package.json").is_some_and(|text| npm_test(&text)) {
        &["npm", "test"]
    } else if dir.join("go.mod").is_file() {
        &["go", "test", "./..."]
    } else if dir.join("pyproject.toml").is_file() || dir.join("pytest.ini").is_file() {
        &["pytest"]
    } else if ["Makefile", "makefile", "GNUmakefile"]
        .iter()
        .filter_map(|name| read(name))
        .any(|text| text.lines().any(|line| line.starts_with("test:")))
    {
        &["make", "test"]
    } else {
        return None;
    };
    Some(argv.iter().map(|word| (*word).to_owned()).collect())
}

/// What the daemon runs when a repository names no test command: git lists a file that is edited
/// or added and left uncommitted, and `--error-unmatch` makes that exit 0 and an unchanged tree
/// exit 1. Nothing is interpolated and no shell runs it.
#[must_use]
pub fn changed_check() -> Vec<String> {
    [
        "git",
        "ls-files",
        "--modified",
        "--deleted",
        "--others",
        "--exclude-standard",
        "--error-unmatch",
        "--",
        ".",
    ]
    .iter()
    .map(|word| (*word).to_owned())
    .collect()
}

/// The Floor's label for a check: the plain words for [`changed_check`], else the command.
#[must_use]
pub fn check_label(argv: &[String]) -> String {
    if argv == changed_check() {
        CHANGED_LABEL.to_owned()
    } else {
        argv.join(" ")
    }
}

/// Whether `package.json` has a `test` script, other than the placeholder `npm init` writes.
fn npm_test(manifest: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(manifest)
        .ok()
        .and_then(|json| json["scripts"]["test"].as_str().map(str::to_owned))
        .is_some_and(|script| !script.contains("no test specified"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_over_the_cap_is_read_only_up_to_it() {
        let dir = tempfile::tempdir().expect("dir");
        let mut manifest = String::from(r#"{"scripts":{"test":"jest"},"pad":""#);
        manifest.push_str(&"x".repeat(1 << 21));
        manifest.push_str("\"}");
        std::fs::write(dir.path().join("package.json"), manifest).expect("manifest");
        assert_eq!(
            suggest_check(dir.path()),
            None,
            "a truncated manifest is not JSON"
        );
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"test":"jest"}}"#,
        )
        .expect("manifest");
        assert_eq!(
            suggest_check(dir.path()),
            Some(vec!["npm".to_owned(), "test".to_owned()])
        );
    }

    /// A FIFO or a link named like a manifest is skipped without being opened.
    #[cfg(unix)]
    #[test]
    fn a_fifo_or_a_link_named_like_a_manifest_is_not_opened() {
        let dir = tempfile::tempdir().expect("dir");
        // This crate forbids unsafe code, so the FIFO comes from the system tool.
        let made = std::process::Command::new("mkfifo")
            .arg(dir.path().join("package.json"))
            .status();
        assert!(made.expect("mkfifo").success());
        std::fs::write(dir.path().join("real.mk"), "test:\n").expect("file");
        std::os::unix::fs::symlink(dir.path().join("real.mk"), dir.path().join("Makefile"))
            .expect("link");
        assert_eq!(
            suggest_check(dir.path()),
            None,
            "falls back and does not block"
        );
    }
}
