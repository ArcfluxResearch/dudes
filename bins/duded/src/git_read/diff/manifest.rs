use std::collections::{BTreeMap, BTreeSet};

use dudes_proto::{
    DIFF_MANIFEST_LIFETIME_MS, DiffAvailability, DiffBinding, DiffContent, DiffEntry, DiffFile,
    DiffFreshness, DiffOmission, DiffOmissionReason, DiffPage, DiffRefusal, DiffResponse,
    MAX_DIFF_CANDIDATES, MAX_DIFF_CONTENT_BYTES, MAX_DIFF_FILES, MAX_DIFF_MANIFEST_BYTES,
    MAX_DIFF_PAGE_BYTES, MAX_DIFF_PAGE_ENTRIES,
};
use dudes_types::Timestamp;
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use super::RepoPath;

/// One immutable candidate discovered by the daemon, never from a request path.
#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    /// Displayed current path, or the baseline path for a deletion.
    pub path: RepoPath,
    /// Previous path for an exact rename.
    pub old_path: Option<RepoPath>,
    /// Public metadata without content.
    pub entry: DiffEntry,
    pub(super) old: Option<super::reader::TrackedObject>,
    pub(super) current: Option<super::reader::TrackedObject>,
    pub(super) observed_current_mode: Option<u32>,
}

/// One selected whole file pair or a finite omission.
#[derive(Clone, Debug)]
pub enum SampledPair {
    /// Complete admitted text.
    Text(DiffFile),
    /// Whole-pair refusal.
    Omitted {
        /// Selected server token.
        entry_id: String,
        /// Finite refusal.
        reason: DiffOmissionReason,
    },
}

/// Serialized response bytes. Accounting and transport consume these exact bytes.
#[derive(Clone, Debug)]
pub struct EncodedResponse(Vec<u8>);

impl EncodedResponse {
    /// The complete response including its JSON envelope and escape expansion.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// One bounded immutable candidate set. Its stored serialization includes all cached metadata.
#[derive(Debug, Serialize)]
pub struct CandidateManifest {
    id: String,
    binding: DiffBinding,
    expires_at: Timestamp,
    candidates: Vec<Candidate>,
    cursors: BTreeMap<String, usize>,
    omitted: u32,
    omitted_lower_bound: bool,
    untracked: u32,
    untracked_lower_bound: bool,
    freshness: DiffFreshness,
    metadata_digest: Option<String>,
}

fn token(seed: &[u8; 32], domain: &[u8], position: usize) -> String {
    let mut hash = Sha256::new();
    hash.update(seed);
    hash.update(domain);
    hash.update(position.to_le_bytes());
    super::hex(&hash.finalize()[..16])
}

fn encoded(value: &impl Serialize, cap: usize) -> Result<EncodedResponse, DiffRefusal> {
    let bytes = serde_json::to_vec(value).map_err(|_| DiffRefusal::Unreadable)?;
    if bytes.len() > cap {
        return Err(DiffRefusal::Unreadable);
    }
    Ok(EncodedResponse(bytes))
}

impl CandidateManifest {
    /// Build from server observations and a newly generated server secret, never browser input.
    ///
    /// # Errors
    /// Refuses an invalid binding or an envelope that cannot fit within the metadata cap.
    pub fn build(
        binding: DiffBinding,
        now: Timestamp,
        mut candidates: Vec<Candidate>,
        untracked: u32,
        enumeration_cut: bool,
        seed: [u8; 32],
    ) -> Result<Self, DiffRefusal> {
        let fingerprint = |value: &str| {
            value.len() == 64
                && value
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        };
        if !fingerprint(&binding.baseline_id) || !fingerprint(&binding.target_id) {
            return Err(DiffRefusal::Unreadable);
        }
        candidates.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then(left.old_path.cmp(&right.old_path))
        });
        if candidates
            .windows(2)
            .any(|pair| pair[0].path == pair[1].path)
        {
            return Err(DiffRefusal::Unreadable);
        }
        let total = candidates.len();
        candidates.truncate(MAX_DIFF_CANDIDATES);
        for (position, candidate) in candidates.iter_mut().enumerate() {
            candidate.entry.path = candidate.path.as_str().to_owned();
            candidate.entry.old_path = candidate
                .old_path
                .as_ref()
                .map(|path| path.as_str().to_owned());
            if super::text::sensitive_path(candidate.path.as_str())
                || candidate
                    .old_path
                    .as_ref()
                    .is_some_and(|path| super::text::sensitive_path(path.as_str()))
            {
                candidate.entry.availability = DiffAvailability::Unavailable {
                    reason: DiffOmissionReason::SensitivePath,
                };
            }
            candidate.entry.entry_id =
                matches!(candidate.entry.availability, DiffAvailability::Available)
                    .then(|| token(&seed, b"entry", position));
        }
        let mut manifest = Self {
            id: token(&seed, b"manifest", 0),
            binding,
            expires_at: now.saturating_add_millis(DIFF_MANIFEST_LIFETIME_MS),
            candidates,
            cursors: BTreeMap::new(),
            omitted: u32::try_from(total.saturating_sub(MAX_DIFF_CANDIDATES)).unwrap_or(u32::MAX),
            omitted_lower_bound: enumeration_cut,
            untracked,
            untracked_lower_bound: enumeration_cut,
            freshness: DiffFreshness::Unknown,
            metadata_digest: None,
        };
        for position in 1..manifest.candidates.len() {
            manifest
                .cursors
                .insert(token(&seed, b"cursor", position), position);
        }
        while serde_json::to_vec(&manifest)
            .map_err(|_| DiffRefusal::Unreadable)?
            .len()
            > MAX_DIFF_MANIFEST_BYTES
        {
            if manifest.candidates.pop().is_none() {
                return Err(DiffRefusal::Unreadable);
            }
            manifest
                .cursors
                .retain(|_, position| *position < manifest.candidates.len());
            manifest.omitted = manifest.omitted.saturating_add(1);
        }
        Ok(manifest)
    }

    /// The opaque manifest identity a request must present.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// When the manifest's fixed lifetime ends.
    #[must_use]
    pub const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    pub(super) fn metadata_digest(&self) -> Option<&str> {
        self.metadata_digest.as_deref()
    }

    pub(super) fn bind_capture(
        &mut self,
        metadata: String,
        omitted: u32,
        omitted_lower_bound: bool,
        untracked_lower_bound: bool,
        freshness: DiffFreshness,
    ) -> Result<(), DiffRefusal> {
        self.metadata_digest = Some(metadata);
        self.omitted = self.omitted.saturating_add(omitted);
        self.omitted_lower_bound |= omitted_lower_bound;
        self.untracked_lower_bound = untracked_lower_bound;
        self.freshness = if self.binding.check_lsn.is_some() {
            freshness
        } else {
            DiffFreshness::Unknown
        };
        while self.metadata_bytes() > MAX_DIFF_MANIFEST_BYTES {
            if self.candidates.pop().is_none() {
                return Err(DiffRefusal::Unreadable);
            }
            self.cursors
                .retain(|_, position| *position < self.candidates.len());
            self.omitted = self.omitted.saturating_add(1);
        }
        Ok(())
    }

    /// The immutable candidate set. No new enumeration occurs on page or content requests.
    #[must_use]
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// The complete retained serialized metadata size, including its binding and cursor map.
    #[must_use]
    pub fn metadata_bytes(&self) -> usize {
        serde_json::to_vec(self).map_or(usize::MAX, |bytes| bytes.len())
    }

    fn cursor(&self, position: usize) -> Option<String> {
        self.cursors
            .iter()
            .find_map(|(token, at)| (*at == position).then(|| token.clone()))
    }

    /// Serialize the first page or one opaque continuation of the cached set.
    ///
    /// # Errors
    /// Refuses an unknown cursor or an invalid envelope.
    pub fn page(&self, cursor: Option<&str>) -> Result<EncodedResponse, DiffRefusal> {
        let start = match cursor {
            Some(cursor) => *self.cursors.get(cursor).ok_or(DiffRefusal::StaleManifest)?,
            None => 0,
        };
        let mut page = DiffPage {
            manifest_id: self.id.clone(),
            binding: self.binding.clone(),
            expires_at: self.expires_at,
            entries: Vec::new(),
            next_cursor: self.cursor(start),
            omitted: self.omitted,
            omitted_lower_bound: self.omitted_lower_bound,
            untracked: self.untracked,
            untracked_lower_bound: self.untracked_lower_bound,
            truncated: self.omitted > 0 || self.omitted_lower_bound,
            freshness: self.freshness,
        };
        for (offset, candidate) in self.candidates[start..]
            .iter()
            .take(MAX_DIFF_PAGE_ENTRIES)
            .enumerate()
        {
            let previous = page.next_cursor.clone();
            page.entries.push(candidate.entry.clone());
            page.next_cursor = self.cursor(start + offset + 1);
            let response = if cursor.is_none() {
                DiffResponse::Manifest(page.clone())
            } else {
                DiffResponse::Page(page.clone())
            };
            if encoded(&response, MAX_DIFF_PAGE_BYTES).is_err() {
                page.entries.pop();
                page.next_cursor = previous;
                break;
            }
        }
        if page.entries.is_empty() && start < self.candidates.len() {
            return Err(DiffRefusal::Unreadable);
        }
        encoded(
            &if cursor.is_none() {
                DiffResponse::Manifest(page)
            } else {
                DiffResponse::Page(page)
            },
            MAX_DIFF_PAGE_BYTES,
        )
    }

    fn selected(&self, ids: &[String]) -> Result<Vec<usize>, DiffRefusal> {
        if ids.is_empty()
            || ids.len() > MAX_DIFF_FILES
            || ids.iter().collect::<BTreeSet<_>>().len() != ids.len()
        {
            return Err(DiffRefusal::InvalidRequest);
        }
        let mut selected = Vec::new();
        for id in ids {
            let position = self
                .candidates
                .iter()
                .position(|candidate| candidate.entry.entry_id.as_deref() == Some(id))
                .ok_or(DiffRefusal::InvalidRequest)?;
            selected.push(position);
        }
        selected.sort_unstable();
        Ok(selected)
    }

    /// Resolve only existing available server tokens before filesystem sampling begins.
    ///
    /// # Errors
    /// Refuses unknown, repeated, unavailable or oversized selections.
    pub fn select(&self, ids: &[String]) -> Result<Vec<&Candidate>, DiffRefusal> {
        Ok(self
            .selected(ids)?
            .into_iter()
            .map(|position| &self.candidates[position])
            .collect())
    }

    /// Validate selections and serialize whole sampled pairs within the content cap.
    ///
    /// # Errors
    /// Refuses unknown, unavailable, repeated, or mixed selected tokens and malformed samples.
    pub fn content(
        &self,
        ids: &[String],
        pairs: Vec<SampledPair>,
    ) -> Result<EncodedResponse, DiffRefusal> {
        let selected = self.selected(ids)?;
        let mut samples = BTreeMap::new();
        for pair in pairs {
            let id = match &pair {
                SampledPair::Text(file) => &file.entry_id,
                SampledPair::Omitted { entry_id, .. } => entry_id,
            }
            .clone();
            if samples.insert(id, pair).is_some() {
                return Err(DiffRefusal::InvalidRequest);
            }
        }
        if samples.len() != ids.len() || samples.keys().any(|id| !ids.contains(id)) {
            return Err(DiffRefusal::InvalidRequest);
        }
        let mut content = DiffContent {
            manifest_id: self.id.clone(),
            binding: self.binding.clone(),
            expires_at: self.expires_at,
            freshness: DiffFreshness::Unknown,
            files: Vec::new(),
            omitted: Vec::new(),
            truncated: false,
        };
        let mut files = Vec::new();
        for position in selected {
            let candidate = &self.candidates[position];
            let id = candidate
                .entry
                .entry_id
                .as_ref()
                .ok_or(DiffRefusal::InvalidRequest)?;
            match samples.remove(id).ok_or(DiffRefusal::InvalidRequest)? {
                SampledPair::Text(file) => {
                    if file.path != candidate.entry.path
                        || file.old_path != candidate.entry.old_path
                        || file.kind != candidate.entry.kind
                    {
                        return Err(DiffRefusal::InvalidRequest);
                    }
                    let reason = super::text::policy(file.old.as_bytes())
                        .and_then(|_| super::text::policy(file.current.as_bytes()))
                        .err()
                        .map(DiffOmissionReason::from);
                    if let Some(reason) = reason {
                        content.omitted.push(DiffOmission {
                            entry_id: id.clone(),
                            reason,
                        });
                    } else {
                        files.push(file);
                    }
                }
                SampledPair::Omitted { entry_id, reason } => {
                    content.omitted.push(DiffOmission { entry_id, reason });
                }
            }
        }
        fill_content(&mut content, files)?;
        content.omitted.sort_by_key(|omission| {
            self.candidates.iter().position(|candidate| {
                candidate.entry.entry_id.as_deref() == Some(&omission.entry_id)
            })
        });
        encoded(&DiffResponse::Content(content), MAX_DIFF_CONTENT_BYTES)
    }
}

fn fill_content(content: &mut DiffContent, files: Vec<DiffFile>) -> Result<(), DiffRefusal> {
    let mut limit_reached = false;
    for file in files {
        let id = file.entry_id.clone();
        if limit_reached {
            content.omitted.push(DiffOmission {
                entry_id: id,
                reason: DiffOmissionReason::ResponseLimit,
            });
            continue;
        }
        content.files.push(file);
        if encoded(
            &DiffResponse::Content(content.clone()),
            MAX_DIFF_CONTENT_BYTES,
        )
        .is_err()
        {
            content.files.pop();
            content.omitted.push(DiffOmission {
                entry_id: id,
                reason: DiffOmissionReason::ResponseLimit,
            });
            content.truncated = true;
            limit_reached = true;
        }
    }
    while encoded(
        &DiffResponse::Content(content.clone()),
        MAX_DIFF_CONTENT_BYTES,
    )
    .is_err()
    {
        let file = content.files.pop().ok_or(DiffRefusal::Unreadable)?;
        content.omitted.push(DiffOmission {
            entry_id: file.entry_id,
            reason: DiffOmissionReason::ResponseLimit,
        });
        content.truncated = true;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dudes_proto::{DiffAvailability, DiffKind, DiffResponse};
    use dudes_types::{AgentId, CardId, GoalId, Seq};

    fn binding() -> DiffBinding {
        DiffBinding {
            card: CardId::NIL,
            seq: Seq(2),
            round: 1,
            goal: GoalId::NIL,
            claimant: AgentId::NIL,
            check_lsn: None,
            baseline_id: "a".repeat(64),
            target_id: "b".repeat(64),
            sampler_version: 1,
            policy_version: 1,
            preexisting_tracked: true,
        }
    }
    fn candidate(n: usize) -> Candidate {
        let path = format!("src/{n:04}.txt");
        Candidate {
            path: RepoPath::from_bytes(path.as_bytes()).unwrap(),
            old_path: None,
            entry: DiffEntry {
                entry_id: None,
                old_path: None,
                path,
                kind: DiffKind::Modified,
                availability: DiffAvailability::Available,
            },
            old: None,
            current: None,
            observed_current_mode: None,
        }
    }
    fn make_manifest(count: usize) -> CandidateManifest {
        CandidateManifest::build(
            binding(),
            Timestamp::UNIX_EPOCH,
            (0..count).rev().map(candidate).collect(),
            3,
            false,
            [7; 32],
        )
        .unwrap()
    }
    fn page(response: &EncodedResponse) -> dudes_proto::DiffPage {
        match serde_json::from_slice(response.bytes()).unwrap() {
            DiffResponse::Manifest(page) | DiffResponse::Page(page) => page,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stable_pages_end_at_the_immutable_cache_cap_and_never_reenumerate() {
        let manifest = make_manifest(300);
        let first = page(&manifest.page(None).unwrap());
        assert_eq!(first.entries.len(), MAX_DIFF_PAGE_ENTRIES);
        assert_eq!(
            (
                first.entries[0].path.as_str(),
                first.omitted,
                first.untracked,
                first.truncated
            ),
            ("src/0000.txt", 44, 3, true)
        );
        let mut entries = first.entries.clone();
        let mut cursor = first.next_cursor;
        while let Some(next) = cursor {
            let next = page(&manifest.page(Some(&next)).unwrap());
            entries.extend(next.entries);
            cursor = next.next_cursor;
        }
        assert_eq!(entries.len(), MAX_DIFF_CANDIDATES);
        assert_eq!(entries.last().unwrap().path, "src/0255.txt");
        assert_eq!(
            manifest.page(Some("foreign")).unwrap_err(),
            DiffRefusal::StaleManifest
        );
        assert_eq!(first.manifest_id.len(), 32);
        assert_eq!(first.expires_at.unix_millis(), 60_000);
        assert_eq!(
            manifest.page(None).unwrap().bytes(),
            manifest.page(None).unwrap().bytes()
        );
    }

    #[test]
    fn selected_content_includes_escaping_and_envelope_in_the_cap() {
        let manifest = make_manifest(8);
        let first = page(&manifest.page(None).unwrap());
        let ids: Vec<_> = first
            .entries
            .iter()
            .map(|entry| entry.entry_id.clone().unwrap())
            .collect();
        let pairs = first
            .entries
            .iter()
            .map(|entry| {
                SampledPair::Text(DiffFile {
                    entry_id: entry.entry_id.clone().unwrap(),
                    old_path: None,
                    path: entry.path.clone(),
                    kind: entry.kind,
                    old: ("\"\\\t".repeat(1000) + "\n").repeat(4),
                    current: ("\"\\\t".repeat(1000) + "\n").repeat(4),
                })
            })
            .collect();
        let encoded = manifest.content(&ids, pairs).unwrap();
        assert!(encoded.bytes().len() <= MAX_DIFF_CONTENT_BYTES);
        let DiffResponse::Content(content) = serde_json::from_slice(encoded.bytes()).unwrap()
        else {
            panic!("content response")
        };
        assert_eq!(content.files.len(), 2);
        assert_eq!(content.omitted.len(), 6);
        assert!(content.truncated);
        assert_eq!(content.omitted[0].reason, DiffOmissionReason::ResponseLimit);
        assert_eq!(content.files[0].old.len(), 12004);
    }

    #[test]
    fn repeated_unknown_and_unavailable_selection_is_refused_before_text() {
        let manifest = make_manifest(1);
        let id = page(&manifest.page(None).unwrap()).entries[0]
            .entry_id
            .clone()
            .unwrap();
        for ids in [vec![], vec![id.clone(), id], vec!["foreign".to_owned()]] {
            assert_eq!(
                manifest.content(&ids, vec![]).unwrap_err(),
                DiffRefusal::InvalidRequest
            );
        }
    }

    #[test]
    fn content_stops_at_the_first_serialized_excess() {
        let manifest = make_manifest(4);
        let first = page(&manifest.page(None).unwrap());
        let ids: Vec<_> = first
            .entries
            .iter()
            .map(|entry| entry.entry_id.clone().unwrap())
            .collect();
        let pairs = first
            .entries
            .iter()
            .enumerate()
            .map(|(n, entry)| {
                SampledPair::Text(DiffFile {
                    entry_id: ids[n].clone(),
                    old_path: None,
                    path: entry.path.clone(),
                    kind: entry.kind,
                    old: if n == 3 {
                        "small".to_owned()
                    } else {
                        ("\"\\\t".repeat(1000) + "\n").repeat(4)
                    },
                    current: ("\"\\\t".repeat(1000) + "\n").repeat(4),
                })
            })
            .collect();
        let DiffResponse::Content(content) =
            serde_json::from_slice(manifest.content(&ids, pairs).unwrap().bytes()).unwrap()
        else {
            panic!("content")
        };
        assert_eq!((content.files.len(), content.omitted.len()), (2, 2));
        assert_eq!(content.omitted[1].entry_id, ids[3]);
    }

    #[test]
    fn content_rechecks_the_shared_whole_pair_text_policy() {
        let single = make_manifest(1);
        let entry = page(&single.page(None).unwrap()).entries.remove(0);
        let id = entry.entry_id.unwrap();
        for (text, reason) in [
            (
                "password='ordinary-example-value'",
                DiffOmissionReason::SensitiveContent,
            ),
            ("source\0text", DiffOmissionReason::Binary),
            ("source\u{202e}text", DiffOmissionReason::UnsupportedText),
        ] {
            let file = DiffFile {
                entry_id: id.clone(),
                old_path: None,
                path: entry.path.clone(),
                kind: entry.kind,
                old: String::new(),
                current: text.to_owned(),
            };
            let DiffResponse::Content(content) = serde_json::from_slice(
                single
                    .content(std::slice::from_ref(&id), vec![SampledPair::Text(file)])
                    .unwrap()
                    .bytes(),
            )
            .unwrap() else {
                panic!("content")
            };
            assert!(content.files.is_empty());
            assert_eq!(content.omitted[0].reason, reason);
        }
    }

    #[test]
    fn sensitive_candidates_get_unavailable_rows_without_tokens() {
        let mut candidate = candidate(0);
        candidate.path = RepoPath::from_bytes(b".env.production").unwrap();
        let manifest = CandidateManifest::build(
            binding(),
            Timestamp::UNIX_EPOCH,
            vec![candidate],
            0,
            false,
            [8; 32],
        )
        .unwrap();
        let entry = page(&manifest.page(None).unwrap()).entries.remove(0);
        assert_eq!(entry.path, ".env.production");
        assert_eq!(entry.entry_id, None);
        assert_eq!(
            entry.availability,
            DiffAvailability::Unavailable {
                reason: DiffOmissionReason::SensitivePath
            }
        );
    }

    #[test]
    fn escaped_metadata_is_capped_as_a_complete_retained_manifest() {
        let candidates = (0..256)
            .map(|n| {
                let mut candidate = candidate(n);
                candidate.path =
                    RepoPath::from_bytes(format!("{n:04}{}", "\"".repeat(196)).as_bytes()).unwrap();
                candidate.old_path = Some(
                    RepoPath::from_bytes(format!("old{n:04}{}", "\"".repeat(193)).as_bytes())
                        .unwrap(),
                );
                candidate
            })
            .collect();
        let manifest = CandidateManifest::build(
            binding(),
            Timestamp::UNIX_EPOCH,
            candidates,
            9,
            true,
            [9; 32],
        )
        .unwrap();
        assert!(manifest.metadata_bytes() <= dudes_proto::MAX_DIFF_MANIFEST_BYTES);
        assert!(manifest.candidates().len() < 256);
        let first = page(&manifest.page(None).unwrap());
        assert!(first.omitted > 0 && first.omitted_lower_bound && first.untracked_lower_bound);
        assert!(manifest.page(None).unwrap().bytes().len() <= dudes_proto::MAX_DIFF_PAGE_BYTES);
    }

    #[test]
    fn canonical_wire_fixtures_are_complete_serialized_responses() {
        let manifest = make_manifest(1);
        let encoded = manifest.page(None).unwrap();
        let first = page(&encoded);
        let entry = &first.entries[0];
        let id = entry.entry_id.clone().unwrap();
        let file = DiffFile {
            entry_id: id.clone(),
            old_path: None,
            path: entry.path.clone(),
            kind: entry.kind,
            old: "old\n".to_owned(),
            current: "new\nhttps://user:pass@example.invalid/repo\n/opt/project/src\n".to_owned(),
        };
        let content = manifest
            .content(&[id], vec![SampledPair::Text(file)])
            .unwrap();
        let content_json: serde_json::Value = serde_json::from_slice(content.bytes()).unwrap();
        assert_eq!(content_json["files"][0]["old"], "old\n");
        assert_eq!(content_json["mode"], "content");
        if let Some(directory) = std::env::var_os("DUDES_DIFF_FIXTURE_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("manifest.json"), encoded.bytes()).unwrap();
            std::fs::write(directory.join("content.json"), content.bytes()).unwrap();
            std::fs::write(
                directory.join("refused.json"),
                serde_json::to_vec(&DiffResponse::Refused {
                    reason: DiffRefusal::Expired,
                })
                .unwrap(),
            )
            .unwrap();
        }
    }

    #[test]
    fn a_missing_check_never_acquires_a_matching_or_changed_freshness_claim() {
        for freshness in [
            DiffFreshness::MatchesReviewSample,
            DiffFreshness::ChangedAfterCheck,
        ] {
            let mut manifest = make_manifest(1);
            manifest
                .bind_capture("c".repeat(64), 0, false, false, freshness)
                .unwrap();
            assert_eq!(
                page(&manifest.page(None).unwrap()).freshness,
                DiffFreshness::Unknown
            );
        }
    }
}
