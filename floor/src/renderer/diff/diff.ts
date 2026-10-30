// SPDX-License-Identifier: MIT OR Apache-2.0

export const DIFF_KINDS = ['added', 'modified', 'deleted', 'renamed', 'type_changed',
  'nested_repository'] as const;
export type DiffKind = (typeof DIFF_KINDS)[number];
export const DIFF_OMISSIONS = ['binary', 'unsupported_text', 'sensitive_path', 'sensitive_content',
  'oversize_file', 'oversize_line', 'unsupported_kind', 'raced', 'unreadable', 'response_limit', 'hard_linked'] as const;
export type DiffOmissionReason = (typeof DIFF_OMISSIONS)[number];
export const DIFF_REFUSALS = ['baseline_unavailable', 'stale_ticket', 'stale_manifest', 'expired', 'invalid_request',
  'disabled', 'busy', 'session_limit', 'unsupported_platform', 'worktree_mismatch', 'unsupported_storage',
  'root_mismatch', 'timed_out', 'git_unavailable', 'unreadable'] as const;
export type DiffRefusalReason = (typeof DIFF_REFUSALS)[number];
export const DIFF_FRESHNESS = ['matches_review_sample', 'changed_after_check', 'unknown'] as const;
export type DiffFreshness = (typeof DIFF_FRESHNESS)[number];

export interface DiffRepositoryBinding {
  readonly repo_id: string | null;
  readonly baseline_id: string;
  readonly target_id: string;
  readonly unavailable: DiffRefusalReason | null;
  readonly preexisting_tracked: boolean;
  readonly preexisting_untracked: boolean;
}
export interface DiffBinding {
  readonly hired_lsn: string | null;
  readonly start_lsn: string | null;
  readonly check_repo: string | null;
  readonly repositories: readonly DiffRepositoryBinding[];
  readonly card: string;
  readonly seq: number;
  readonly round: number;
  readonly goal: string;
  readonly claimant: string;
  readonly check_lsn: string | null;
  readonly baseline_id: string;
  readonly target_id: string;
  readonly policy_version: number;
  readonly sampler_version: number;
  readonly preexisting_tracked: boolean;
  readonly preexisting_untracked: boolean;
}
export interface DiffEntry {
  readonly repo_id: string | null;
  readonly entry_id: string | null;
  readonly old_path: string | null;
  readonly path: string;
  readonly kind: DiffKind;
  readonly availability: { readonly state: 'available' }
    | { readonly state: 'unavailable'; readonly reason: DiffOmissionReason };
}
export interface DiffPage {
  readonly mode: 'manifest' | 'page';
  readonly manifest_id: string;
  readonly binding: DiffBinding;
  readonly expires_at: number;
  readonly entries: readonly DiffEntry[];
  readonly next_cursor: string | null;
  readonly omitted: number;
  readonly omitted_lower_bound: boolean;
  readonly untracked: number;
  readonly untracked_lower_bound: boolean;
  readonly truncated: boolean;
  readonly freshness: DiffFreshness;
}
export interface DiffFile {
  readonly repo_id: string | null;
  readonly entry_id: string;
  readonly old_path: string | null;
  readonly path: string;
  readonly kind: DiffKind;
  readonly old: string;
  readonly current: string;
}
export interface DiffContent {
  readonly mode: 'content';
  readonly manifest_id: string;
  readonly binding: DiffBinding;
  readonly expires_at: number;
  readonly freshness: DiffFreshness;
  readonly files: readonly DiffFile[];
  readonly omitted: readonly { readonly entry_id: string; readonly reason: DiffOmissionReason }[];
  readonly truncated: boolean;
}
export type DiffResponse = DiffPage | DiffContent | { readonly mode: 'refused'; readonly reason: DiffRefusalReason };
export type DiffRequest = { readonly mode: 'manifest'; readonly card: string; readonly expected_seq: number;
  readonly repositories?: { readonly scope: 'declared' } | { readonly scope: 'repository'; readonly repo_id: string } }
  | { readonly mode: 'page'; readonly manifest_id: string; readonly cursor: string }
  | { readonly mode: 'content'; readonly manifest_id: string; readonly entries: readonly string[] };
export type DiffOutcome = { readonly kind: 'response'; readonly response: DiffResponse }
  | { readonly kind: 'forbidden' | 'unavailable' | 'unknown' };
export interface DiffTransport {
  request(request: DiffRequest): Promise<DiffOutcome>;
  generation(): number;
}

const UTF8 = new TextEncoder();
const REPO_ID = /^[a-z][a-z0-9_-]{0,31}$/;
const ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;
const OPAQUE = /^[0-9a-f]{32}$/;
const DIGEST = /^[0-9a-f]{64}$/;
const FORBIDDEN = /[\p{Cc}\p{Cf}\u2028\u2029]/u;
const SOURCE_FORBIDDEN = /(?:(?![\t\n\r])[\p{Cc}\p{Cf}])|[\u2028\u2029]/u;

function exactly(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const own = Object.keys(value);
  return own.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}
function count(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 && value <= 4_294_967_295;
}
function safeCount(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}
function opaque(value: unknown): value is string {
  return typeof value === 'string' && OPAQUE.test(value);
}
function oneOf<T extends string>(value: unknown, choices: readonly T[]): value is T {
  return typeof value === 'string' && choices.includes(value as T);
}
function wellFormed(value: string): boolean {
  return Array.from(value).every((character) => character.length === 2
    || character.charCodeAt(0) < 0xd800 || character.charCodeAt(0) > 0xdfff);
}
function path(value: unknown): value is string {
  return typeof value === 'string' && wellFormed(value) && UTF8.encode(value).length <= 200
    && !FORBIDDEN.test(value) && !value.includes('\\')
    && value.split('/').every((part) => part !== '' && part !== '.' && part !== '..' && part.toLowerCase() !== '.git');
}
function source(value: unknown): value is string {
  return typeof value === 'string' && wellFormed(value) && UTF8.encode(value).length <= 16_384
    && !SOURCE_FORBIDDEN.test(value)
    && value.split(/\r\n|\r|\n/).every((line) => UTF8.encode(line).length <= 4_096);
}
function nullableRepo(value: unknown): value is string | null {
  return value === null || (typeof value === 'string' && REPO_ID.test(value));
}
function nullableLsn(value: unknown): value is string | null {
  return value === null || (typeof value === 'string' && /^(?:0|[1-9][0-9]{0,19})$/.test(value)
    && BigInt(value) <= 18_446_744_073_709_551_615n);
}
function repositoryBinding(value: unknown): DiffRepositoryBinding | null {
  if (!exactly(value, ['repo_id', 'baseline_id', 'target_id', 'unavailable', 'preexisting_tracked', 'preexisting_untracked'])
    || !nullableRepo(value.repo_id) || typeof value.baseline_id !== 'string' || !DIGEST.test(value.baseline_id)
    || typeof value.target_id !== 'string' || !DIGEST.test(value.target_id)
    || !(value.unavailable === null || oneOf(value.unavailable, DIFF_REFUSALS))
    || typeof value.preexisting_tracked !== 'boolean' || typeof value.preexisting_untracked !== 'boolean') return null;
  return Object.freeze({ repo_id: value.repo_id, baseline_id: value.baseline_id, target_id: value.target_id,
    unavailable: value.unavailable, preexisting_tracked: value.preexisting_tracked, preexisting_untracked: value.preexisting_untracked });
}
function binding(value: unknown): DiffBinding | null {
  if (!exactly(value, ['card', 'seq', 'round', 'goal', 'claimant', 'hired_lsn', 'start_lsn', 'check_repo', 'repositories', 'check_lsn', 'baseline_id', 'target_id',
    'policy_version', 'sampler_version', 'preexisting_tracked', 'preexisting_untracked'])
    || typeof value.card !== 'string' || !ID.test(value.card) || !safeCount(value.seq) || !count(value.round)
    || typeof value.goal !== 'string' || !ID.test(value.goal) || typeof value.claimant !== 'string' || !ID.test(value.claimant)
    || !nullableLsn(value.check_lsn) || !nullableLsn(value.hired_lsn) || !nullableLsn(value.start_lsn)
    || !nullableRepo(value.check_repo) || !Array.isArray(value.repositories) || value.repositories.length < 1 || value.repositories.length > 8
    || typeof value.baseline_id !== 'string' || !DIGEST.test(value.baseline_id)
    || typeof value.target_id !== 'string' || !DIGEST.test(value.target_id) || !count(value.policy_version)
    || !count(value.sampler_version) || typeof value.preexisting_tracked !== 'boolean'
    || typeof value.preexisting_untracked !== 'boolean') return null;
  const repositories = value.repositories.map(repositoryBinding);
  if (!repositories.every((row) => row !== null) || new Set(repositories.map((row) => row.repo_id)).size !== repositories.length) return null;
  if (value.hired_lsn === null ? repositories.length !== 1 || repositories[0]?.repo_id !== null || value.check_repo !== null
    : value.start_lsn === null || value.check_repo === null || repositories.some((row) => row.repo_id === null)) return null;
  return Object.freeze({ hired_lsn: value.hired_lsn, start_lsn: value.start_lsn, check_repo: value.check_repo,
    repositories: Object.freeze(repositories), card: value.card, seq: value.seq, round: value.round, goal: value.goal,
    claimant: value.claimant, check_lsn: value.check_lsn, baseline_id: value.baseline_id,
    target_id: value.target_id, policy_version: value.policy_version, sampler_version: value.sampler_version,
    preexisting_tracked: value.preexisting_tracked, preexisting_untracked: value.preexisting_untracked });
}
function entry(value: unknown): DiffEntry | null {
  if (!exactly(value, ['repo_id', 'entry_id', 'old_path', 'path', 'kind', 'availability']) || !nullableRepo(value.repo_id) || !path(value.path)
    || !(value.old_path === null || path(value.old_path)) || !oneOf(value.kind, DIFF_KINDS)) return null;
  const available = value.availability;
  if (exactly(available, ['state']) && available.state === 'available' && opaque(value.entry_id)) {
    return Object.freeze({ repo_id: value.repo_id, entry_id: value.entry_id, old_path: value.old_path, path: value.path,
      kind: value.kind, availability: Object.freeze({ state: 'available' }) });
  }
  if (exactly(available, ['state', 'reason']) && available.state === 'unavailable' && value.entry_id === null
    && oneOf(available.reason, DIFF_OMISSIONS)) {
    return Object.freeze({ repo_id: value.repo_id, entry_id: null, old_path: value.old_path, path: value.path, kind: value.kind,
      availability: Object.freeze({ state: 'unavailable', reason: available.reason }) });
  }
  return null;
}
function file(value: unknown): DiffFile | null {
  if (!exactly(value, ['repo_id', 'entry_id', 'old_path', 'path', 'kind', 'old', 'current']) || !opaque(value.entry_id)
    || !nullableRepo(value.repo_id) || !path(value.path) || !(value.old_path === null || path(value.old_path)) || !oneOf(value.kind, DIFF_KINDS)
    || !source(value.old) || !source(value.current)) return null;
  return Object.freeze({ repo_id: value.repo_id, entry_id: value.entry_id, old_path: value.old_path, path: value.path,
    kind: value.kind, old: value.old, current: value.current });
}

export function sameDiffBinding(left: DiffBinding, right: DiffBinding): boolean {
  return JSON.stringify(left) === JSON.stringify(right);
}

export function decodeDiffResponse(body: string): DiffResponse | null {
  const length = UTF8.encode(body).length;
  if (length > 131_072) return null;
  let value: unknown;
  try { value = JSON.parse(body); } catch { return null; }
  if (exactly(value, ['mode', 'reason']) && value.mode === 'refused' && oneOf(value.reason, DIFF_REFUSALS)) {
    return Object.freeze({ mode: 'refused', reason: value.reason });
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  const raw = value as Record<string, unknown>;
  const parsedBinding = binding(raw.binding);
  if (parsedBinding === null || !opaque(raw.manifest_id) || typeof raw.expires_at !== 'number'
    || !Number.isSafeInteger(raw.expires_at) || raw.expires_at < 0 || !oneOf(raw.freshness, DIFF_FRESHNESS)
    || (parsedBinding.check_lsn === null && raw.freshness !== 'unknown') || typeof raw.truncated !== 'boolean') return null;
  if ((raw.mode === 'manifest' || raw.mode === 'page') && length <= 65_536
    && exactly(raw, ['mode', 'manifest_id', 'binding', 'expires_at', 'entries', 'next_cursor', 'omitted',
      'omitted_lower_bound', 'untracked', 'untracked_lower_bound', 'truncated', 'freshness'])
    && Array.isArray(raw.entries) && raw.entries.length <= 64 && (raw.next_cursor === null || opaque(raw.next_cursor))
    && count(raw.omitted) && count(raw.untracked) && typeof raw.omitted_lower_bound === 'boolean'
    && typeof raw.untracked_lower_bound === 'boolean') {
    const entries = raw.entries.map(entry);
    if (!entries.every((row) => row !== null)) return null;
    if (entries.some((row) => !parsedBinding.repositories.some((repo) => repo.repo_id === row.repo_id && repo.unavailable === null))
      || new Set(entries.map((row) => `${row.repo_id ?? ''}:${row.path}`)).size !== entries.length) return null;
    const ids = entries.flatMap((row) => row.entry_id === null ? [] : [row.entry_id]);
    if (new Set(ids).size !== ids.length) return null;
    return Object.freeze({ mode: raw.mode, manifest_id: raw.manifest_id, binding: parsedBinding,
      expires_at: raw.expires_at, entries: Object.freeze(entries), next_cursor: raw.next_cursor,
      omitted: raw.omitted, omitted_lower_bound: raw.omitted_lower_bound, untracked: raw.untracked,
      untracked_lower_bound: raw.untracked_lower_bound, truncated: raw.truncated, freshness: raw.freshness });
  }
  if (raw.mode === 'content' && exactly(raw, ['mode', 'manifest_id', 'binding', 'expires_at', 'freshness', 'files', 'omitted', 'truncated'])
    && Array.isArray(raw.files) && raw.files.length <= 8 && Array.isArray(raw.omitted) && raw.omitted.length <= 8
    && raw.files.length + raw.omitted.length <= 8) {
    const files = raw.files.map(file);
    if (!files.every((row) => row !== null)) return null;
    if (files.some((row) => !parsedBinding.repositories.some((repo) => repo.repo_id === row.repo_id && repo.unavailable === null))) return null;
    const omitted: { readonly entry_id: string; readonly reason: DiffOmissionReason }[] = [];
    for (const row of raw.omitted) {
      if (!exactly(row, ['entry_id', 'reason']) || !opaque(row.entry_id) || !oneOf(row.reason, DIFF_OMISSIONS)) return null;
      omitted.push(Object.freeze({ entry_id: row.entry_id, reason: row.reason }));
    }
    const ids = [...files.map((row) => row.entry_id), ...omitted.map((row) => row.entry_id)];
    if (new Set(ids).size !== ids.length) return null;
    return Object.freeze({ mode: 'content', manifest_id: raw.manifest_id, binding: parsedBinding,
      expires_at: raw.expires_at, freshness: raw.freshness, files: Object.freeze(files),
      omitted: Object.freeze(omitted), truncated: raw.truncated });
  }
  return null;
}

export async function postDiff(request: DiffRequest, signedIn: boolean,
  fetch: (url: string, init: RequestInit) => Promise<{ readonly status: number; text(): Promise<string> }>): Promise<DiffOutcome> {
  if (!signedIn) return Object.freeze({ kind: 'forbidden' });
  let status: number;
  let body: string;
  try {
    const reply = await fetch('/floor/diff', {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(request),
    });
    status = reply.status;
    body = await reply.text();
  } catch {
    return Object.freeze({ kind: 'unavailable' });
  }
  if (status === 200) {
    const response = decodeDiffResponse(body);
    return response === null ? Object.freeze({ kind: 'unknown' }) : Object.freeze({ kind: 'response', response });
  }
  if (status === 403) return Object.freeze({ kind: 'forbidden' });
  // The daemon routes this path only while the switches are on, so an unrouted POST means they were turned off.
  if (status === 404) return Object.freeze({ kind: 'response', response: Object.freeze({ mode: 'refused', reason: 'disabled' }) });
  return Object.freeze({ kind: status === 503 ? 'unavailable' : 'unknown' });
}

export interface LineChanges {
  /** Zero-based lines of the old text that the current text no longer has. */
  readonly removed: ReadonlySet<number>;
  /** Zero-based lines of the current text that the old text did not have. */
  readonly added: ReadonlySet<number>;
}

/** The most line pairs compared, which keeps the comparison well under a frame for the sizes the viewer is allowed to show. */
const MAX_LINE_PAIRS = 4_000_000;

const linesOf = (text: string): string[] => text === '' ? [] : text.split(/\r\n|\r|\n/).slice(0, /[\r\n]$/.test(text) ? -1 : undefined);

/**
 * Which lines were taken out and which were put in, by the longest run of lines the two texts share. A file too big to
 * compare cheaply gets `null`, and the viewer then shows it without colour.
 */
export function changedLines(oldText: string, currentText: string): LineChanges | null {
  const before = linesOf(oldText);
  const after = linesOf(currentText);
  let start = 0;
  while (start < before.length && start < after.length && before[start] === after[start]) start += 1;
  let endBefore = before.length;
  let endAfter = after.length;
  while (endBefore > start && endAfter > start && before[endBefore - 1] === after[endAfter - 1]) { endBefore -= 1; endAfter -= 1; }
  const n = endBefore - start;
  const m = endAfter - start;
  if (n * m > MAX_LINE_PAIRS) return null;
  const width = m + 1;
  const table = new Uint32Array((n + 1) * width);
  for (let i = n - 1; i >= 0; i -= 1) {
    for (let j = m - 1; j >= 0; j -= 1) {
      table[i * width + j] = before[start + i] === after[start + j]
        ? table[(i + 1) * width + j + 1]! + 1
        : Math.max(table[(i + 1) * width + j]!, table[i * width + j + 1]!);
    }
  }
  const removed = new Set<number>();
  const added = new Set<number>();
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (before[start + i] === after[start + j]) { i += 1; j += 1; }
    else if (table[(i + 1) * width + j]! >= table[i * width + j + 1]!) { removed.add(start + i); i += 1; }
    else { added.add(start + j); j += 1; }
  }
  for (; i < n; i += 1) removed.add(start + i);
  for (; j < m; j += 1) added.add(start + j);
  return { removed, added };
}
