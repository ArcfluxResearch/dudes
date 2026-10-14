//! Where `install.sh` puts a release, and how to tell that a running `dudes` is one of its installs.

use std::path::{Path, PathBuf};

/// The three programs `install.sh` puts in `<prefix>/bin`, by their exact names.
pub(crate) const PROGRAMS: [&str; 3] = ["duded", "dudes", "dudes-hook"];

/// The notice files `install.sh` puts in `<prefix>/share/dudes`.
pub(crate) const NOTICES: [&str; 3] = ["LICENSE-APACHE", "LICENSE-MIT", "THIRD-PARTY.md"];

fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// The prefix `install.sh` installed `executable` into, or why it is not one of its installs.
///
/// The executable is canonicalized and must not be a symbolic link, all three programs must be
/// regular files in `<prefix>/bin`, and the notice files must be in `<prefix>/share/dudes`.
pub(crate) fn installed_prefix(executable: &Path) -> Result<PathBuf, String> {
    let refusal = || {
        format!(
            "this copy of dudes ({}) was not put there by install.sh, such as a source build, a cargo install or a link to another copy",
            executable.display()
        )
    };
    if std::fs::symlink_metadata(executable).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(refusal());
    }
    let real = executable.canonicalize().map_err(|_| refusal())?;
    let bin = real
        .parent()
        .filter(|bin| bin.file_name() == Some("bin".as_ref()))
        .ok_or_else(refusal)?;
    let prefix = bin.parent().ok_or_else(refusal)?;
    let complete = PROGRAMS.iter().all(|name| regular_file(&bin.join(name)))
        && NOTICES
            .iter()
            .all(|name| regular_file(&prefix.join("share/dudes").join(name)));
    if complete {
        Ok(prefix.to_path_buf())
    } else {
        Err(refusal())
    }
}
