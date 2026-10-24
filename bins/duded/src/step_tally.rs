//! A turn's step counts for its `TurnEnded`, kept as the steps arrive and holding no names.

use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use dudes_proto::StepDigest;
use dudes_providers::steps::{StepEvent, StepKind};

/// Distinct file hashes remembered. Past it, further edits count as one file each, so the figure
/// stays an upper bound on distinct files and memory stays fixed.
const MAX_FILES: usize = 4096;

/// The argument names engines put an edited file's path under.
const PATH_KEYS: [&str; 2] = ["file_path", "path"];

#[derive(Debug, Default)]
pub(crate) struct StepTally {
    digest: StepDigest,
    files: BTreeSet<u64>,
}

impl StepTally {
    pub(crate) fn count(&mut self, event: &StepEvent) {
        let StepEvent::Open { step, .. } = event else {
            return;
        };
        let slot = match step.kind {
            StepKind::Command => &mut self.digest.command,
            StepKind::Read => &mut self.digest.read,
            StepKind::Edit => &mut self.digest.edit,
            StepKind::Write => &mut self.digest.write,
            StepKind::Search => &mut self.digest.search,
            StepKind::Web => &mut self.digest.web,
            StepKind::Plan => &mut self.digest.plan,
            StepKind::Subagent => &mut self.digest.subagent,
            StepKind::Mcp => &mut self.digest.mcp,
            StepKind::Other => &mut self.digest.other,
        };
        *slot = slot.saturating_add(1);
        if !matches!(step.kind, StepKind::Edit | StepKind::Write) {
            return;
        }
        let path = step
            .args
            .iter()
            .find(|(key, _)| PATH_KEYS.contains(&key.as_str()))
            .map(|(_, value)| value);
        let new = match path {
            Some(path) if self.files.len() < MAX_FILES || self.files.contains(&hash(path)) => {
                self.files.insert(hash(path))
            }
            _ => true,
        };
        if new {
            self.digest.files_edited = self.digest.files_edited.saturating_add(1);
        }
    }

    /// The digest, or `None` when no step was read: an engine whose steps are not read has
    /// reported nothing, which is not the same as a turn that took none.
    pub(crate) fn digest(&self) -> Option<StepDigest> {
        (self.digest.total() > 0).then_some(self.digest)
    }
}

fn hash(path: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use dudes_providers::steps::Step;
    use dudes_types::SessionId;

    use super::*;

    fn open(kind: StepKind, path: Option<&str>) -> StepEvent {
        StepEvent::Open {
            session: SessionId::new("s"),
            step: Step {
                id: "t".into(),
                parent: None,
                kind,
                tool: "Tool".into(),
                args: path
                    .map(|path| ("file_path".to_owned(), path.to_owned()))
                    .into_iter()
                    .collect(),
                edit: None,
                plan: Vec::new(),
            },
        }
    }

    #[test]
    fn counts_each_kind_and_distinct_edited_files() {
        let mut tally = StepTally::default();
        assert_eq!(tally.digest(), None);
        for event in [
            open(StepKind::Command, None),
            open(StepKind::Command, None),
            open(StepKind::Edit, Some("/a.rs")),
            open(StepKind::Edit, Some("/a.rs")),
            open(StepKind::Write, Some("/b.rs")),
            open(StepKind::Read, Some("/c.rs")),
        ] {
            tally.count(&event);
        }
        let digest = tally.digest().unwrap();
        assert_eq!(
            (digest.command, digest.edit, digest.write, digest.read),
            (2, 2, 1, 1)
        );
        assert_eq!(digest.files_edited, 2);
    }
}
