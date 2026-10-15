use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::{CandidateManifest, SampledPair};
#[cfg(unix)]
use super::{files::Budget, shadow::ContentShadow};
#[cfg(unix)]
use crate::predicate::PredicateCaptured;
use dudes_proto::{
    DiffAvailability, DiffBinding, DiffEntry, DiffFile, DiffFreshness, DiffKind,
    DiffOmissionReason, DiffRefusal, MAX_DIFF_CANDIDATES, MAX_DIFF_SIDE_BYTES,
};
#[cfg(unix)]
use dudes_providers::launch::{EnvVar, LaunchInput, LaunchSpec};
use dudes_types::GitCommit;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::{Candidate, RepoPath, RootIdentity};
use crate::predicate::PredicateRunner;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub(super) struct ObjectId(String);

impl ObjectId {
    fn parse(bytes: &[u8]) -> Option<Self> {
        super::super::parse::is_object_id(bytes)
            .then(|| Self(String::from_utf8(bytes.to_vec()).expect("hexadecimal object id")))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct TrackedObject {
    id: ObjectId,
    mode: u32,
}

struct Listing {
    files: BTreeMap<RepoPath, TrackedObject>,
    tracked: BTreeSet<Vec<u8>>,
    omitted: u32,
    cut: bool,
}

fn listing(output: &[u8], index: bool, cut: bool) -> Result<Listing, DiffRefusal> {
    let mut parsed = Listing {
        files: BTreeMap::new(),
        tracked: BTreeSet::new(),
        omitted: 0,
        cut,
    };
    let mut records = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    let last = records.pop().ok_or(DiffRefusal::Unreadable)?;
    if !cut && !last.is_empty() {
        return Err(DiffRefusal::Unreadable);
    }
    for record in records {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or(DiffRefusal::Unreadable)?;
        let header = record[..tab]
            .split(|byte| *byte == b' ')
            .collect::<Vec<_>>();
        if header.len() != 3
            || header[0].len() != 6
            || !header[0].iter().all(|byte| matches!(byte, b'0'..=b'7'))
        {
            return Err(DiffRefusal::Unreadable);
        }
        let mut mode = u32::from_str_radix(
            std::str::from_utf8(header[0]).map_err(|_| DiffRefusal::Unreadable)?,
            8,
        )
        .map_err(|_| DiffRefusal::Unreadable)?;
        let id =
            ObjectId::parse(header[if index { 1 } else { 2 }]).ok_or(DiffRefusal::Unreadable)?;
        if index {
            if !matches!(header[2], b"0" | b"1" | b"2" | b"3") {
                return Err(DiffRefusal::Unreadable);
            }
            if header[2] != b"0" {
                mode = 0;
            }
        } else if !matches!(header[1], b"blob" | b"commit" | b"tree") {
            return Err(DiffRefusal::Unreadable);
        }
        let name = &record[tab + 1..];
        parsed.tracked.insert(name.to_vec());
        let Some(path) = RepoPath::from_bytes(name) else {
            parsed.omitted = parsed.omitted.saturating_add(1);
            continue;
        };
        let entry = TrackedObject { id, mode };
        if let Some(previous) = parsed.files.insert(path.clone(), entry.clone()) {
            if !index || mode != 0 {
                return Err(DiffRefusal::Unreadable);
            }
            parsed.files.insert(
                path,
                TrackedObject {
                    mode: 0,
                    ..previous
                },
            );
        }
    }
    Ok(parsed)
}

fn status_dirty(output: &[u8], cut: bool, success: bool) -> Result<bool, DiffRefusal> {
    let mut parts = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    let last = parts.pop().ok_or(DiffRefusal::Unreadable)?;
    if !cut && !last.is_empty() {
        return Err(DiffRefusal::Unreadable);
    }
    let mut records = parts.into_iter();
    let mut dirty = false;
    while let Some(record) = records.next() {
        if record.len() < 4 || record[2] != b' ' || !relative_name(&record[3..]) {
            return Err(DiffRefusal::Unreadable);
        }
        let status = &record[..2];
        if status == b"??" {
            continue;
        }
        if status == b"  "
            || !status
                .iter()
                .all(|byte| matches!(byte, b' ' | b'M' | b'A' | b'D' | b'R' | b'C' | b'U' | b'T'))
        {
            return Err(DiffRefusal::Unreadable);
        }
        if (status.contains(&b'R') || status.contains(&b'C'))
            && !records.next().is_some_and(relative_name)
        {
            return Err(DiffRefusal::Unreadable);
        }
        dirty = true;
    }
    if dirty {
        Ok(true)
    } else if success && !cut {
        Ok(false)
    } else {
        Err(DiffRefusal::Unreadable)
    }
}
fn relative_name(name: &[u8]) -> bool {
    !name.starts_with(b"/")
        && !name.split(|byte| *byte == b'/').any(|part| {
            part.is_empty() || part == b"." || part == b".." || part.eq_ignore_ascii_case(b".git")
        })
}

/// Round-start content comparison input. It contains no file bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BaselineCapture {
    /// Identity of the opened recorded root.
    pub root: RootIdentity,
    /// Full round-start HEAD commit.
    pub head: GitCommit,
    /// Exact tracked-change flag observed at the round start.
    pub preexisting_tracked: bool,
}

/// One bounded complete-or-incomplete scope sample. No per-file hash reaches the Floor.
#[derive(Debug)]
pub struct ReviewCapture {
    /// Ordered changed or unavailable candidate metadata.
    pub candidates: Vec<Candidate>,
    /// Hash of every sampled eligible pair, including unchanged pairs.
    pub digest: String,
    /// Whether the entire eligible scope was sampled without a cap or refusal.
    pub complete: bool,
    /// Omitted eligible pairs, not a count of uninspected changed files.
    pub omitted: u32,
    /// Whether the omitted count is only a lower bound.
    pub omitted_lower_bound: bool,
    /// Purely untracked names, counted without being disclosed.
    pub untracked: u32,
    /// Whether untracked enumeration stopped early.
    pub untracked_lower_bound: bool,
    metadata_digest: String,
}

impl ReviewCapture {
    /// Mint a new immutable manifest after the caller rechecks the exact live binding.
    ///
    /// # Errors
    /// Refuses unavailable randomness or an invalid bounded envelope.
    pub fn into_manifest(
        self,
        binding: DiffBinding,
        now: dudes_types::Timestamp,
        freshness: DiffFreshness,
    ) -> Result<CandidateManifest, DiffRefusal> {
        if binding.policy_version != super::TEXT_POLICY_VERSION
            || binding.sampler_version != super::SAMPLER_VERSION
        {
            return Err(DiffRefusal::StaleManifest);
        }
        let mut seed = [0_u8; 32];
        getrandom::fill(&mut seed).map_err(|_| DiffRefusal::Unreadable)?;
        let complete = self.complete;
        let metadata = self.metadata_digest;
        let omitted = self.omitted;
        let omitted_lower_bound = self.omitted_lower_bound;
        let untracked_lower_bound = self.untracked_lower_bound;
        let mut manifest = CandidateManifest::build(
            binding,
            now,
            self.candidates,
            self.untracked,
            omitted_lower_bound,
            seed,
        )?;
        manifest.bind_capture(
            metadata,
            omitted,
            omitted_lower_bound,
            untracked_lower_bound,
            if complete {
                freshness
            } else {
                DiffFreshness::Unknown
            },
        )?;
        Ok(manifest)
    }
}

/// The unrouted content reader. All child programs use the daemon-owned runner.
pub struct DiffReader {
    git: super::super::GitReader,
    scratch: PathBuf,
}

impl DiffReader {
    /// Use the daemon's runner/PATH and a private content-only scratch directory.
    #[must_use]
    pub fn new(runner: Arc<dyn PredicateRunner>, path: Option<OsString>, scratch: &Path) -> Self {
        Self {
            git: super::super::GitReader::new(runner, path, scratch.join("metadata")),
            scratch: scratch.join("content"),
        }
    }

    /// Reclaim only verified owned content shadows before admitting this run's readers.
    ///
    /// # Errors
    /// Refuses unsupported or expired cleanup work without recursively following a pathname.
    pub fn reclaim_scratch(&self, cancelled: Arc<AtomicBool>) -> Result<usize, DiffRefusal> {
        #[cfg(unix)]
        {
            ContentShadow::reclaim(&self.scratch, &Budget::new(cancelled))
        }
        #[cfg(not(unix))]
        {
            let _ = cancelled;
            Err(DiffRefusal::UnsupportedPlatform)
        }
    }

    /// Capture exact round-start identity and tracked-change state before any turn is issued.
    ///
    /// # Errors
    /// Refuses an incomplete observation rather than guessing the dirty-start flag.
    pub fn capture_baseline(
        &self,
        tree: &super::super::Tree,
        cancelled: Arc<AtomicBool>,
    ) -> Result<BaselineCapture, DiffRefusal> {
        #[cfg(unix)]
        {
            let budget = Budget::new(cancelled);
            let shadow = self.shadow(tree, None, &budget)?;
            let mut baseline = BaselineCapture {
                root: shadow.root().identity(),
                head: shadow.head(),
                preexisting_tracked: false,
            };
            baseline.preexisting_tracked = self.metadata_dirty(tree, &shadow, &budget)?;
            Ok(baseline)
        }
        #[cfg(not(unix))]
        {
            let _ = (tree, cancelled);
            Err(DiffRefusal::UnsupportedPlatform)
        }
    }

    /// Sample the bounded eligible scope without reading any browser-selected pathname.
    ///
    /// # Errors
    /// Refuses a changed root, unavailable baseline, or failed finite read.
    pub fn capture_review(
        &self,
        tree: &super::super::Tree,
        baseline: &BaselineCapture,
        cancelled: Arc<AtomicBool>,
    ) -> Result<ReviewCapture, DiffRefusal> {
        #[cfg(unix)]
        {
            let budget = Budget::new(cancelled);
            let shadow = self.shadow(tree, Some(baseline.root), &budget)?;
            self.observe(&shadow, baseline, &budget)
        }
        #[cfg(not(unix))]
        {
            let _ = (tree, baseline, cancelled);
            Err(DiffRefusal::UnsupportedPlatform)
        }
    }

    /// Read selected current sides at content time and return only complete admitted JSON bytes.
    ///
    /// # Errors
    /// Refuses stale metadata or invalid opaque selections before returning repository data.
    pub fn read_content(
        &self,
        tree: &super::super::Tree,
        baseline: &BaselineCapture,
        manifest: &CandidateManifest,
        ids: &[String],
        cancelled: Arc<AtomicBool>,
    ) -> Result<super::EncodedResponse, DiffRefusal> {
        let selected = manifest.select(ids)?;
        #[cfg(unix)]
        {
            let budget = Budget::new(cancelled);
            let shadow = self.shadow(tree, Some(baseline.root), &budget)?;
            if manifest.metadata_digest() != Some(metadata_digest(&shadow).as_str()) {
                return Err(DiffRefusal::StaleManifest);
            }
            let mut pairs = Vec::new();
            for candidate in selected {
                let sample = self.sample(&shadow, candidate.clone(), &budget)?;
                if sample.candidate.entry.kind != candidate.entry.kind
                    || sample.candidate.observed_current_mode != candidate.observed_current_mode
                    || matches!(
                        sample.reason,
                        Some(DiffOmissionReason::UnsupportedKind | DiffOmissionReason::Raced)
                    )
                {
                    return Err(DiffRefusal::StaleManifest);
                }
                let id = candidate
                    .entry
                    .entry_id
                    .clone()
                    .ok_or(DiffRefusal::InvalidRequest)?;
                pairs.push(match sample.reason {
                    Some(reason) => SampledPair::Omitted {
                        entry_id: id,
                        reason,
                    },
                    None => SampledPair::Text(DiffFile {
                        entry_id: id,
                        old_path: candidate.entry.old_path.clone(),
                        path: candidate.entry.path.clone(),
                        kind: candidate.entry.kind,
                        old: String::from_utf8(sample.old).map_err(|_| DiffRefusal::Unreadable)?,
                        current: String::from_utf8(sample.current)
                            .map_err(|_| DiffRefusal::Unreadable)?,
                    }),
                });
            }
            shadow.verify(&budget)?;
            manifest.content(ids, pairs)
        }
        #[cfg(not(unix))]
        {
            let _ = (tree, baseline, cancelled, selected);
            Err(DiffRefusal::UnsupportedPlatform)
        }
    }

    #[cfg(unix)]
    fn shadow(
        &self,
        tree: &super::super::Tree,
        expected: Option<RootIdentity>,
        budget: &Budget,
    ) -> Result<ContentShadow, DiffRefusal> {
        let name = format!(
            "{}-{}",
            std::process::id(),
            self.git
                .next
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        ContentShadow::build(&self.scratch, &name, tree, expected, budget)
    }

    #[cfg(unix)]
    fn metadata_dirty(
        &self,
        tree: &super::super::Tree,
        content: &ContentShadow,
        budget: &Budget,
    ) -> Result<bool, DiffRefusal> {
        content.verify(budget)?;
        let metadata = self.git.shadow(tree).map_err(|_| DiffRefusal::Unreadable)?;
        if metadata_head(&metadata, budget)? != content.head() {
            return Err(DiffRefusal::Unreadable);
        }
        let index = metadata_file(&metadata, "index", super::super::shadow::INDEX_MAX, budget)?
            .unwrap_or_default();
        if super::hex(&Sha256::digest(&index)) != content.index_digest() {
            return Err(DiffRefusal::Unreadable);
        }
        let program = self
            .git
            .program_with_timeout(
                metadata.work_tree(),
                budget.remaining()?.min(super::super::COMMAND_DEADLINE),
            )
            .map_err(|_| DiffRefusal::GitUnavailable)?;
        let spec = super::super::launch(
            &self.git,
            &program,
            &metadata,
            &["status", "--porcelain=v1", "-z", "--ignore-submodules=all"],
        );
        let result = self
            .git
            .runner
            .run_captured(
                &spec,
                budget.remaining()?.min(super::super::COMMAND_DEADLINE),
                super::super::STDOUT_CAP,
            )
            .map_err(|_| DiffRefusal::GitUnavailable)?;
        content.verify(budget)?;
        let dirty = match result {
            PredicateCaptured::Finished {
                timed_out: true, ..
            } => return Err(DiffRefusal::TimedOut),
            PredicateCaptured::Finished {
                exit,
                stdout,
                truncated,
                ..
            } => status_dirty(&stdout, truncated, exit == Some(0))?,
            PredicateCaptured::Swept => return Err(DiffRefusal::Unreadable),
        };
        if !dirty && (content.has_gitlinks() || content.has_hidden_entries()) {
            return Err(DiffRefusal::BaselineUnavailable);
        }
        Ok(dirty)
    }

    #[cfg(unix)]
    fn run(
        &self,
        shadow: &ContentShadow,
        args: &[&str],
        budget: &Budget,
        cap: usize,
    ) -> Result<Output, DiffRefusal> {
        shadow.verify(budget)?;
        let program = self
            .git
            .program_with_timeout(
                shadow.root().path(),
                budget.remaining()?.min(super::super::COMMAND_DEADLINE),
            )
            .map_err(|_| DiffRefusal::GitUnavailable)?;
        let mut env = self.git.base_env();
        env.extend([
            EnvVar::new("GIT_DIR", shadow.dir()),
            EnvVar::new("GIT_INDEX_FILE", shadow.dir().join("index")),
            EnvVar::new("GIT_WORK_TREE", shadow.dir()),
        ]);
        let mut command = vec![
            program.into_os_string(),
            OsString::from("--no-pager"),
            OsString::from("--no-optional-locks"),
        ];
        command.extend(args.iter().map(OsString::from));
        let spec = LaunchSpec {
            argv: command,
            env,
            cwd: shadow.dir().to_path_buf(),
            stdin: LaunchInput::Null,
        };
        let result = self.git.runner.run_captured(
            &spec,
            budget.remaining()?.min(super::super::COMMAND_DEADLINE),
            cap,
        );
        budget.check()?;
        shadow.verify(budget)?;
        match result {
            Ok(PredicateCaptured::Finished {
                timed_out: true, ..
            }) => Err(DiffRefusal::TimedOut),
            Ok(PredicateCaptured::Finished {
                exit,
                stdout,
                truncated,
                ..
            }) => Ok(Output {
                exit,
                bytes: stdout,
                cut: truncated,
            }),
            Ok(PredicateCaptured::Swept) => Err(DiffRefusal::Unreadable),
            Err(_) => Err(DiffRefusal::GitUnavailable),
        }
    }

    #[cfg(unix)]
    fn sample(
        &self,
        shadow: &ContentShadow,
        mut candidate: Candidate,
        budget: &Budget,
    ) -> Result<Sample, DiffRefusal> {
        let mut sample = Sample {
            candidate: candidate.clone(),
            old: Vec::new(),
            current: Vec::new(),
            old_present: candidate.old.is_some(),
            current_present: false,
            current_mode: 0,
            reason: None,
        };
        if super::text::sensitive_path(candidate.path.as_str())
            || candidate
                .old_path
                .as_ref()
                .is_some_and(|path| super::text::sensitive_path(path.as_str()))
        {
            sample.reason = Some(DiffOmissionReason::SensitivePath);
            return Ok(sample);
        }
        if candidate
            .old
            .as_ref()
            .is_some_and(|file| !regular(file.mode))
            || candidate
                .current
                .as_ref()
                .is_some_and(|file| !regular(file.mode))
        {
            sample.reason = Some(DiffOmissionReason::UnsupportedKind);
            return Ok(sample);
        }
        if let Some(old) = &candidate.old {
            let blob = self.run(
                shadow,
                &["cat-file", "blob", &old.id.0],
                budget,
                MAX_DIFF_SIDE_BYTES + 1,
            )?;
            if blob.exit != Some(0) && !blob.cut {
                return Err(DiffRefusal::BaselineUnavailable);
            }
            if blob.cut || blob.bytes.len() > MAX_DIFF_SIDE_BYTES {
                sample.reason = Some(DiffOmissionReason::OversizeFile);
                return Ok(sample);
            }
            sample.old = blob.bytes;
        }
        match shadow.root().read_file(&candidate.path, budget) {
            Ok(Some(file)) => {
                sample.current = file.bytes;
                sample.current_mode = file.mode;
                sample.current_present = true;
            }
            Ok(None) => {}
            Err(reason) => {
                sample.reason = Some(reason);
                return Ok(sample);
            }
        }
        budget.check()?;
        sample.reason = super::text::policy(&sample.old)
            .and_then(|_| super::text::policy(&sample.current))
            .err()
            .map(DiffOmissionReason::from);
        candidate.entry.kind = match (sample.old_present, sample.current_present) {
            (false, true) => DiffKind::Added,
            (true, false) => DiffKind::Deleted,
            (true, true) if candidate.old_path.is_some() => DiffKind::Renamed,
            _ => DiffKind::Modified,
        };
        candidate.observed_current_mode = sample.current_present.then_some(sample.current_mode);
        sample.candidate = candidate;
        Ok(sample)
    }

    #[cfg(unix)]
    fn observe(
        &self,
        shadow: &ContentShadow,
        baseline: &BaselineCapture,
        budget: &Budget,
    ) -> Result<ReviewCapture, DiffRefusal> {
        let base = baseline.head.to_string();
        let old = self.run(
            shadow,
            &["ls-tree", "-r", "-z", &base],
            budget,
            super::super::STDOUT_CAP,
        )?;
        if old.exit != Some(0) && !old.cut {
            return Err(DiffRefusal::BaselineUnavailable);
        }
        let current = self.run(
            shadow,
            &["ls-files", "--stage", "-z"],
            budget,
            super::super::STDOUT_CAP,
        )?;
        if current.exit != Some(0) && !current.cut {
            return Err(DiffRefusal::Unreadable);
        }
        let old = listing(&old.bytes, false, old.cut)?;
        let current = listing(&current.bytes, true, current.cut)?;
        let tracked = old
            .tracked
            .union(&current.tracked)
            .cloned()
            .collect::<BTreeSet<_>>();
        let mut paths = old
            .files
            .keys()
            .chain(current.files.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        // A cut baseline listing cannot prove a path is new, so it is counted as omitted.
        let unknown_baseline = if old.cut {
            let before = paths.len();
            paths.retain(|path| old.files.contains_key(path));
            before - paths.len()
        } else {
            0
        };
        let invalid = tracked
            .iter()
            .filter(|path| RepoPath::from_bytes(path).is_none())
            .count();
        let omitted = u32::try_from(
            invalid
                .saturating_add(unknown_baseline)
                .saturating_add(paths.len().saturating_sub(MAX_DIFF_CANDIDATES)),
        )
        .unwrap_or(u32::MAX);
        let cut = old.cut || current.cut;
        let (untracked, untracked_lower_bound) = if cut {
            (0, true)
        } else {
            shadow.root().untracked(&tracked, budget)?
        };
        let mut samples = Vec::new();
        for path in paths.into_iter().take(MAX_DIFF_CANDIDATES) {
            let old = old.files.get(&path).cloned();
            let current = current.files.get(&path).cloned();
            let kind = if old.as_ref().is_some_and(|file| !regular(file.mode))
                || current.as_ref().is_some_and(|file| !regular(file.mode))
            {
                DiffKind::TypeChanged
            } else {
                DiffKind::Modified
            };
            let candidate = Candidate {
                entry: DiffEntry {
                    entry_id: None,
                    old_path: None,
                    path: path.as_str().to_owned(),
                    kind,
                    availability: DiffAvailability::Available,
                },
                path,
                old_path: None,
                old,
                current,
                observed_current_mode: None,
            };
            let sample = self.sample(shadow, candidate, budget)?;
            samples.push(sample);
        }
        let mut review = ReviewCapture {
            candidates: Vec::new(),
            digest: String::new(),
            complete: omitted == 0 && !cut && !untracked_lower_bound,
            omitted,
            omitted_lower_bound: cut,
            untracked,
            untracked_lower_bound,
            metadata_digest: metadata_digest(shadow),
        };
        review.apply_samples(samples)?;
        shadow.verify(budget)?;
        Ok(review)
    }
}

impl ReviewCapture {
    fn apply_samples(&mut self, mut samples: Vec<Sample>) -> Result<(), DiffRefusal> {
        pair_renames(&mut samples);
        samples.sort_by(|left, right| left.candidate.path.cmp(&right.candidate.path));
        let mut hash = Sha256::new();
        hash.update(b"bounded-review-sample-v1");
        for mut sample in samples {
            hash.update(
                serde_json::to_vec(&(
                    &sample.candidate.path,
                    &sample.candidate.old_path,
                    &sample.candidate.old,
                    sample.candidate.current.as_ref().map(|file| file.mode),
                    sample.current_mode,
                    sample.reason,
                ))
                .map_err(|_| DiffRefusal::Unreadable)?,
            );
            if let Some(reason) = sample.reason {
                self.complete = false;
                sample.candidate.entry.availability = DiffAvailability::Unavailable { reason };
            } else {
                hash.update(Sha256::digest(&sample.old));
                hash.update(Sha256::digest(&sample.current));
            }
            if sample.changed() || sample.reason.is_some() {
                self.candidates.push(sample.candidate);
            }
        }
        self.digest = super::hex(&hash.finalize());
        Ok(())
    }
}

#[cfg(unix)]
fn metadata_file(
    shadow: &super::super::shadow::Shadow,
    name: &str,
    cap: u64,
    budget: &Budget,
) -> Result<Option<Vec<u8>>, DiffRefusal> {
    use rustix::fs::{Mode, OFlags, openat};
    use rustix::io::Errno;
    use std::fs::File;
    use std::io::Read as _;
    budget.check()?;
    let mut parent = super::files::open_dir(rustix::fs::CWD, shadow.dir())
        .map_err(|_| DiffRefusal::Unreadable)?;
    let mut parts = name.split('/').peekable();
    while let Some(part) = parts.next() {
        if parts.peek().is_some() {
            parent = match super::files::open_dir(&parent, part) {
                Ok(parent) => parent,
                Err(Errno::NOENT) => return Ok(None),
                Err(_) => return Err(DiffRefusal::Unreadable),
            };
            continue;
        }
        let fd = match openat(
            &parent,
            part,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(_) => return Err(DiffRefusal::Unreadable),
        };
        let mut file = File::from(fd);
        let stamp = super::files::Stamp::file(&file)?;
        let meta = file.metadata().map_err(|_| DiffRefusal::Unreadable)?;
        if !meta.is_file() || meta.len() > cap {
            return Err(DiffRefusal::Unreadable);
        }
        let mut bytes = Vec::new();
        (&mut file)
            .take(cap + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| DiffRefusal::Unreadable)?;
        if bytes.len() as u64 > cap || stamp != super::files::Stamp::file(&file)? {
            return Err(DiffRefusal::Unreadable);
        }
        budget.check()?;
        return Ok(Some(bytes));
    }
    Err(DiffRefusal::Unreadable)
}
#[cfg(unix)]
fn metadata_head(
    shadow: &super::super::shadow::Shadow,
    budget: &Budget,
) -> Result<GitCommit, DiffRefusal> {
    match shadow.head() {
        super::super::parse::Head::Detached(id) => {
            GitCommit::parse(std::str::from_utf8(id).map_err(|_| DiffRefusal::Unreadable)?)
                .map_err(|_| DiffRefusal::Unreadable)
        }
        super::super::parse::Head::Branch(branch) => {
            let reference = format!("refs/heads/{branch}");
            if let Some(bytes) = metadata_file(
                shadow,
                &reference,
                super::super::shadow::LOOSE_REF_MAX,
                budget,
            )? {
                return GitCommit::parse(
                    std::str::from_utf8(bytes.strip_suffix(b"\n").unwrap_or(&bytes))
                        .map_err(|_| DiffRefusal::Unreadable)?,
                )
                .map_err(|_| DiffRefusal::Unreadable);
            }
            let bytes = metadata_file(
                shadow,
                "packed-refs",
                super::super::shadow::PACKED_REFS_MAX,
                budget,
            )?
            .ok_or(DiffRefusal::Unreadable)?;
            for line in bytes.split(|byte| *byte == b'\n') {
                if line.is_empty() || line.starts_with(b"#") || line.starts_with(b"^") {
                    continue;
                }
                let Some(space) = line.iter().position(|byte| *byte == b' ') else {
                    return Err(DiffRefusal::Unreadable);
                };
                if line[space + 1..] == *reference.as_bytes() {
                    return GitCommit::parse(
                        std::str::from_utf8(&line[..space]).map_err(|_| DiffRefusal::Unreadable)?,
                    )
                    .map_err(|_| DiffRefusal::Unreadable);
                }
            }
            Err(DiffRefusal::Unreadable)
        }
    }
}

fn regular(mode: u32) -> bool {
    matches!(mode, 0o100_644 | 0o100_755)
}
#[cfg(unix)]
fn metadata_digest(shadow: &ContentShadow) -> String {
    let mut hash = Sha256::new();
    hash.update(b"bounded-review-metadata-v1");
    hash.update(shadow.head().to_string().as_bytes());
    hash.update(shadow.index_digest().as_bytes());
    hash.update(shadow.root().identity().device.to_le_bytes());
    hash.update(shadow.root().identity().inode.to_le_bytes());
    super::hex(&hash.finalize())
}
#[cfg(unix)]
struct Output {
    exit: Option<i32>,
    bytes: Vec<u8>,
    cut: bool,
}
struct Sample {
    candidate: Candidate,
    old: Vec<u8>,
    current: Vec<u8>,
    old_present: bool,
    current_present: bool,
    current_mode: u32,
    reason: Option<DiffOmissionReason>,
}
impl Sample {
    fn changed(&self) -> bool {
        self.old_present != self.current_present
            || self.old != self.current
            || self.old_present
                && self.current_present
                && self
                    .candidate
                    .old
                    .as_ref()
                    .is_some_and(|file| file.mode != self.current_mode)
            || self.candidate.old_path.is_some()
    }
}
fn pair_renames(samples: &mut Vec<Sample>) {
    let mut pairs = Vec::new();
    for (old_pos, old) in samples.iter().enumerate() {
        if old.reason.is_some()
            || !old.old_present
            || old.current_present
            || old.candidate.current.is_some()
        {
            continue;
        }
        let Some(old_object) = &old.candidate.old else {
            continue;
        };
        let matches = samples
            .iter()
            .enumerate()
            .filter(|(_, new)| {
                new.reason.is_none()
                    && !new.old_present
                    && new.current_present
                    && new
                        .candidate
                        .current
                        .as_ref()
                        .is_some_and(|object| object.id == old_object.id)
            })
            .map(|(position, _)| position)
            .collect::<Vec<_>>();
        let same_old = samples
            .iter()
            .filter(|other| {
                other.old_present
                    && !other.current_present
                    && other
                        .candidate
                        .old
                        .as_ref()
                        .is_some_and(|object| object.id == old_object.id)
            })
            .count();
        if matches.len() == 1 && same_old == 1 {
            pairs.push((old_pos, matches[0]));
        }
    }
    let mut removed = BTreeSet::new();
    for (old, new) in pairs {
        let old_path = samples[old].candidate.path.clone();
        samples[new].old = std::mem::take(&mut samples[old].old);
        samples[new].old_present = true;
        samples[new].candidate.old = samples[old].candidate.old.take();
        samples[new].candidate.old_path = Some(old_path.clone());
        samples[new].candidate.entry.old_path = Some(old_path.as_str().to_owned());
        samples[new].candidate.entry.kind = DiffKind::Renamed;
        removed.insert(old);
    }
    let mut position = 0;
    samples.retain(|_| {
        let keep = !removed.contains(&position);
        position += 1;
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn object_names_accept_only_full_literal_ids() {
        assert_eq!(ObjectId::parse(ID.as_bytes()).unwrap().0, ID);
        for bad in [
            "HEAD",
            "HEAD:path",
            "--help",
            "0123",
            "ABCDEF0123456789abcdef0123456789abcdef0123",
        ] {
            assert_eq!(ObjectId::parse(bad.as_bytes()), None);
        }
    }

    #[test]
    fn tree_and_index_metadata_keep_typed_modes_and_private_raw_names() {
        let tree = format!(
            "100644 blob {ID}\tsrc/main.rs\0{0} blob {ID}\t.env\0",
            "120000"
        );
        let parsed = listing(tree.as_bytes(), false, false).unwrap();
        assert_eq!(
            parsed.files[&RepoPath::from_bytes(b"src/main.rs").unwrap()].mode,
            0o100_644
        );
        assert_eq!(
            (parsed.files.len(), parsed.omitted, parsed.cut),
            (2, 0, false)
        );
        let index = format!("100755 {ID} 0\tnew.rs\0");
        assert_eq!(
            listing(index.as_bytes(), true, false).unwrap().files
                [&RepoPath::from_bytes(b"new.rs").unwrap()]
                .mode,
            0o100_755
        );
        let hostile = format!("100644 {ID} 0\t../outside\0");
        let parsed = listing(hostile.as_bytes(), true, false).unwrap();
        assert_eq!(
            (parsed.files.len(), parsed.omitted, parsed.tracked.len()),
            (0, 1, 1)
        );
    }

    #[test]
    fn partial_or_malformed_metadata_never_becomes_a_complete_candidate() {
        let output = format!("100644 blob {ID}\tok\0partial");
        let parsed = listing(output.as_bytes(), false, true).unwrap();
        assert_eq!((parsed.files.len(), parsed.cut), (1, true));
        assert!(listing(output.as_bytes(), false, false).is_err());
        assert!(listing(b"100644 blob HEAD\tok\0", false, false).is_err());
    }

    #[test]
    fn only_complete_successful_strict_status_can_establish_a_clean_baseline() {
        assert_eq!(status_dirty(b"", false, true), Ok(false));
        assert_eq!(status_dirty(b"?? untracked\0", false, true), Ok(false));
        assert_eq!(status_dirty(b" M tracked\0", false, true), Ok(true));
        assert_eq!(
            status_dirty(b"R  renamed\0original\0", false, true),
            Ok(true)
        );
        assert_eq!(status_dirty(b" M tracked\0partial", true, false), Ok(true));
        for (bytes, cut, success) in [
            (b"?? untracked\0".as_slice(), true, true),
            (b"".as_slice(), false, false),
            (b"bad\0", false, true),
            (b" M ../outside\0", false, true),
            (b"R  renamed\0", false, true),
        ] {
            assert_eq!(
                status_dirty(bytes, cut, success),
                Err(DiffRefusal::Unreadable)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn program_probe_uses_only_the_remaining_content_budget() {
        struct Probe(std::sync::Mutex<Vec<std::time::Duration>>);
        impl PredicateRunner for Probe {
            fn run(
                &self,
                _spec: &dudes_providers::launch::LaunchSpec,
                _timeout: std::time::Duration,
            ) -> std::io::Result<crate::predicate::PredicateOutcome> {
                Err(std::io::Error::other("unexpected uncaptured probe"))
            }
            fn run_captured(
                &self,
                _spec: &dudes_providers::launch::LaunchSpec,
                timeout: std::time::Duration,
                _cap: usize,
            ) -> std::io::Result<crate::predicate::PredicateCaptured> {
                self.0.lock().unwrap().push(timeout);
                Ok(crate::predicate::PredicateCaptured::Finished {
                    exit: Some(0),
                    stdout: b"git version 2.39.3\n".to_vec(),
                    truncated: false,
                    timed_out: false,
                })
            }
            fn sweep(&self) {}
        }
        let root = tempfile::tempdir().unwrap();
        let probe = Arc::new(Probe(std::sync::Mutex::new(Vec::new())));
        let reader = DiffReader::new(
            probe.clone(),
            std::env::var_os("PATH"),
            &root.path().join("scratch"),
        );
        let timeout = std::time::Duration::from_millis(5);
        assert!(
            reader
                .git
                .program_with_timeout(root.path(), timeout)
                .is_ok()
        );
        assert_eq!(*probe.0.lock().unwrap(), vec![timeout]);
    }
}
