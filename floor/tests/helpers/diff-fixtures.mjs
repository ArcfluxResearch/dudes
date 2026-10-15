// SPDX-License-Identifier: MIT OR Apache-2.0

export const DIFF_CARD = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
export const DIFF_ENTRY = 'e'.repeat(32);
export const DIFF_MANIFEST = 'a'.repeat(32);
export const diffBinding = (fields = {}) => ({
  card: DIFF_CARD, seq: 4, round: 2, goal: '01JBQ8XK2M3N4P5Q6R7S8T9VWY',
  claimant: '01JBQ8XK2M3N4P5Q6R7S8T9VWZ', check_lsn: '42', baseline_id: 'b'.repeat(64),
  target_id: 'c'.repeat(64), policy_version: 1, sampler_version: 1, preexisting_tracked: false, ...fields,
});
export const diffEntry = (fields = {}) => ({
  entry_id: DIFF_ENTRY, old_path: null, path: 'src/example.rs', kind: 'modified',
  availability: { state: 'available' }, ...fields,
});
export const diffPage = (fields = {}) => ({
  mode: 'manifest', manifest_id: DIFF_MANIFEST, binding: diffBinding(), expires_at: 1_792_016_460_000,
  entries: [diffEntry()], next_cursor: null, omitted: 0, omitted_lower_bound: false,
  untracked: 2, untracked_lower_bound: false, truncated: false, freshness: 'matches_review_sample', ...fields,
});
export const diffFile = (fields = {}) => ({
  entry_id: DIFF_ENTRY, old_path: null, path: 'src/example.rs', kind: 'modified',
  old: 'fn count() -> u8 {\n    1\n}\n', current: 'fn count() -> u8 {\n    2\n}\n', ...fields,
});
export const diffContent = (fields = {}) => ({
  mode: 'content', manifest_id: DIFF_MANIFEST, binding: diffBinding(), expires_at: 1_792_016_460_000,
  freshness: 'matches_review_sample', files: [diffFile()], omitted: [], truncated: false, ...fields,
});
