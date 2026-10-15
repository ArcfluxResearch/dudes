// SPDX-License-Identifier: MIT OR Apache-2.0

export const DIFF_KINDS = ['added', 'modified', 'deleted', 'renamed', 'type_changed'] as const;
export type DiffKind = (typeof DIFF_KINDS)[number];
export const DIFF_OMISSIONS = ['binary', 'unsupported_text', 'sensitive_path', 'sensitive_content',
  'oversize_file', 'oversize_line', 'unsupported_kind', 'raced', 'unreadable', 'response_limit'] as const;
export type DiffOmissionReason = (typeof DIFF_OMISSIONS)[number];
export const DIFF_REFUSALS = ['baseline_unavailable', 'stale_ticket', 'stale_manifest', 'expired', 'invalid_request',
  'disabled', 'busy', 'session_limit', 'unsupported_platform', 'worktree_mismatch', 'unsupported_storage',
  'root_mismatch', 'timed_out', 'git_unavailable', 'unreadable'] as const;
export type DiffRefusalReason = (typeof DIFF_REFUSALS)[number];
export const DIFF_FRESHNESS = ['matches_review_sample', 'changed_after_check', 'unknown'] as const;
export type DiffFreshness = (typeof DIFF_FRESHNESS)[number];

export interface DiffBinding {
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
}
export interface DiffEntry {
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
export type DiffRequest = { readonly mode: 'manifest'; readonly card: string; readonly expected_seq: number }
  | { readonly mode: 'page'; readonly manifest_id: string; readonly cursor: string }
  | { readonly mode: 'content'; readonly manifest_id: string; readonly entries: readonly string[] };
export type DiffOutcome = { readonly kind: 'response'; readonly response: DiffResponse }
  | { readonly kind: 'forbidden' | 'unavailable' | 'unknown' };
export interface DiffTransport {
  request(request: DiffRequest): Promise<DiffOutcome>;
  generation(): number;
}

const UTF8 = new TextEncoder();
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
function binding(value: unknown): DiffBinding | null {
  if (!exactly(value, ['card', 'seq', 'round', 'goal', 'claimant', 'check_lsn', 'baseline_id', 'target_id',
    'policy_version', 'sampler_version', 'preexisting_tracked'])
    || typeof value.card !== 'string' || !ID.test(value.card) || !safeCount(value.seq) || !count(value.round)
    || typeof value.goal !== 'string' || !ID.test(value.goal) || typeof value.claimant !== 'string' || !ID.test(value.claimant)
    || !(value.check_lsn === null || (typeof value.check_lsn === 'string'
      && /^(?:0|[1-9][0-9]{0,19})$/.test(value.check_lsn) && BigInt(value.check_lsn) <= 18_446_744_073_709_551_615n))
    || typeof value.baseline_id !== 'string' || !DIGEST.test(value.baseline_id)
    || typeof value.target_id !== 'string' || !DIGEST.test(value.target_id) || !count(value.policy_version)
    || !count(value.sampler_version) || typeof value.preexisting_tracked !== 'boolean') return null;
  return Object.freeze({ card: value.card, seq: value.seq, round: value.round, goal: value.goal,
    claimant: value.claimant, check_lsn: value.check_lsn, baseline_id: value.baseline_id,
    target_id: value.target_id, policy_version: value.policy_version, sampler_version: value.sampler_version,
    preexisting_tracked: value.preexisting_tracked });
}
function entry(value: unknown): DiffEntry | null {
  if (!exactly(value, ['entry_id', 'old_path', 'path', 'kind', 'availability']) || !path(value.path)
    || !(value.old_path === null || path(value.old_path)) || !oneOf(value.kind, DIFF_KINDS)) return null;
  const available = value.availability;
  if (exactly(available, ['state']) && available.state === 'available' && opaque(value.entry_id)) {
    return Object.freeze({ entry_id: value.entry_id, old_path: value.old_path, path: value.path,
      kind: value.kind, availability: Object.freeze({ state: 'available' }) });
  }
  if (exactly(available, ['state', 'reason']) && available.state === 'unavailable' && value.entry_id === null
    && oneOf(available.reason, DIFF_OMISSIONS)) {
    return Object.freeze({ entry_id: null, old_path: value.old_path, path: value.path, kind: value.kind,
      availability: Object.freeze({ state: 'unavailable', reason: available.reason }) });
  }
  return null;
}
function file(value: unknown): DiffFile | null {
  if (!exactly(value, ['entry_id', 'old_path', 'path', 'kind', 'old', 'current']) || !opaque(value.entry_id)
    || !path(value.path) || !(value.old_path === null || path(value.old_path)) || !oneOf(value.kind, DIFF_KINDS)
    || !source(value.old) || !source(value.current)) return null;
  return Object.freeze({ entry_id: value.entry_id, old_path: value.old_path, path: value.path,
    kind: value.kind, old: value.old, current: value.current });
}

export function sameDiffBinding(left: DiffBinding, right: DiffBinding): boolean {
  return (Object.keys(left) as (keyof DiffBinding)[]).every((key) => left[key] === right[key]);
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
