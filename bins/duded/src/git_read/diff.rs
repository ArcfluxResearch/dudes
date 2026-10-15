//! Bounded checkout content sampling and immutable selection builders.
#![cfg_attr(not(unix), allow(dead_code, unused_imports))]

#[cfg(unix)]
mod files;
mod manifest;
mod path;
mod reader;
#[cfg(unix)]
mod shadow;
mod text;

pub use manifest::{Candidate, CandidateManifest, EncodedResponse, SampledPair};
pub use path::RepoPath;
pub use reader::{BaselineCapture, DiffReader, ReviewCapture};

/// Text and sensitive-content recognizer version used by this reader.
pub const TEXT_POLICY_VERSION: u32 = text::POLICY_VERSION;
/// Ordered eligible-pair sampling algorithm version.
pub const SAMPLER_VERSION: u32 = 1;

/// Identity of the opened recorded checkout root. Replacing its pathname never substitutes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RootIdentity {
    /// Filesystem device.
    pub device: u64,
    /// Directory inode.
    pub inode: u64,
}

impl From<text::TextRefusal> for dudes_proto::DiffOmissionReason {
    fn from(reason: text::TextRefusal) -> Self {
        match reason {
            text::TextRefusal::OversizeFile => Self::OversizeFile,
            text::TextRefusal::OversizeLine => Self::OversizeLine,
            text::TextRefusal::Binary => Self::Binary,
            text::TextRefusal::UnsupportedText => Self::UnsupportedText,
            text::TextRefusal::SensitiveContent => Self::SensitiveContent,
        }
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut value, "{byte:02x}").expect("formatting a String");
    }
    value
}
