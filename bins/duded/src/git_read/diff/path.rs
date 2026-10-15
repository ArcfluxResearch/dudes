use dudes_proto::MAX_DIFF_PATH_BYTES;
use serde::Serialize;

/// A server-observed repository-relative path. Browser requests never construct these.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct RepoPath(String);

impl RepoPath {
    /// Validate a whole path without truncation. Invalid display names receive no token.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (path, cut) = super::super::text::path(bytes)?;
        if cut
            || path.len() > MAX_DIFF_PATH_BYTES
            || path.contains('\\')
            || path
                .split('/')
                .any(|part| part == "." || part.eq_ignore_ascii_case(".git"))
        {
            return None;
        }
        Some(Self(path))
    }

    /// The validated repository-relative spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_whole_relative_names_without_administrative_components() {
        assert_eq!(
            RepoPath::from_bytes("src/日本語.rs".as_bytes())
                .unwrap()
                .as_str(),
            "src/日本語.rs"
        );
        for path in [
            b"".as_slice(),
            b"/a",
            b"../a",
            b"a/../b",
            b"a//b",
            b"a/./b",
            b".git/config",
            b"a/.GIT/b",
            b"a\\b",
            b"a\nb",
            b"a\0b",
            b"a/",
            &[0xff],
        ] {
            assert_eq!(RepoPath::from_bytes(path), None, "{path:?}");
        }
        assert!(RepoPath::from_bytes(&[b'x'; MAX_DIFF_PATH_BYTES]).is_some());
        assert!(RepoPath::from_bytes(&vec![b'x'; MAX_DIFF_PATH_BYTES + 1]).is_none());
    }
}
