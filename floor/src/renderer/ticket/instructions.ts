// SPDX-License-Identifier: MIT OR Apache-2.0

export interface InstructionWatermark {
  readonly through_lsn: string;
  readonly set_digest: string;
}

export interface InstructionReviewSummary {
  readonly watermark: InstructionWatermark;
  readonly pending_comparison: boolean;
  readonly intervals: number;
  readonly changed_paths: number;
  readonly gaps: number;
}

export type InstructionContext = { readonly kind: 'owned_worktree'; readonly lease: string }
  | { readonly kind: 'shared_project' | 'unshared_project' | 'unresolved' }
  | { readonly kind: 'shared_input'; readonly source: 'profile' | 'ancestor' | 'import' | 'explicit_file' };

export type InstructionPath = { readonly encoding: 'utf8'; readonly value: string }
  | { readonly encoding: 'unix_bytes'; readonly hex: string }
  | { readonly encoding: 'windows_wide'; readonly units: readonly number[] };

export interface InstructionShownPath {
  readonly path: InstructionPath;
  readonly display: string;
  readonly display_truncated: boolean;
  readonly context: InstructionContext;
}

export interface InstructionInterval {
  readonly kind: 'managed' | 'attached_process' | 'attached_turn';
  readonly agent: string;
  readonly turn: string | null;
  readonly attachment_lsn: string | null;
  readonly issue_lsn: string | null;
  readonly baseline_lsn: string | null;
  readonly missing_start: boolean;
  readonly card: string;
  readonly goal: string;
  readonly assignment_lsn: string;
  readonly seq_at_start: number;
  readonly finished_lsn: string | null;
  readonly end_lsn: string | null;
  readonly sampled_at: number | null;
  readonly recovery: boolean;
}

const INSTRUCTION_GAPS = Object.freeze([
  'configuration_unknown', 'plugin_unknown', 'policy_unknown', 'root_marker_unknown', 'fallback_unknown',
  'profile_unknown', 'declaration_missing', 'path_unresolved', 'read_failed', 'changed_during_read',
  'import_unresolved', 'import_cycle', 'participant_changed', 'declaration_changed', 'uninventoried_member',
  'input_limit', 'participant_coverage_unknown', 'participant_limit', 'ancestor_limit', 'descendant_limit', 'import_limit', 'symlink_limit',
  'source_bytes_limit', 'deadline', 'metadata_limit', 'missing_baseline', 'unrecorded_launch_boundary',
  'cleanup_unconfirmed', 'attached_turn_start_missing', 'ending_boundary_missing',
] as const);
export type InstructionGapReason = (typeof INSTRUCTION_GAPS)[number];

export interface InstructionGap {
  readonly reason: InstructionGapReason;
  readonly context: InstructionContext;
  readonly agent: string | null;
  readonly path: InstructionPath | null;
  readonly display: string | null;
  readonly display_truncated: boolean;
}

export interface InstructionReview {
  readonly watermark: InstructionWatermark;
  readonly pending_comparison: boolean;
  readonly intervals: readonly InstructionInterval[];
  readonly changed_paths: readonly InstructionShownPath[];
  readonly gaps: readonly InstructionGap[];
}

const ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;
const UTF8 = new TextEncoder();
function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}
function count(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 && value <= 4_294_967_295;
}
function lsn(value: unknown): value is string {
  return typeof value === 'string' && /^(?:0|[1-9][0-9]{0,19})$/.test(value) && BigInt(value) <= 18_446_744_073_709_551_615n;
}
function nullableLsn(value: unknown): value is string | null {
  return value === null || lsn(value);
}
function nullableId(value: unknown): value is string | null {
  return value === null || (typeof value === 'string' && ID.test(value));
}
function display(value: unknown): value is string {
  return typeof value === 'string' && UTF8.encode(value).length <= 4_096
    && !/[\u0000-\u001f\u007f-\u009f\u202a-\u202e\u2066-\u2069]/u.test(value);
}

function decodeInstructionWatermark(value: unknown): InstructionWatermark | null {
  if (!object(value) || Object.keys(value).length !== 2 || !lsn(value.through_lsn)
    || typeof value.set_digest !== 'string' || !/^[0-9a-f]{64}$/.test(value.set_digest)) return null;
  return Object.freeze({ through_lsn: value.through_lsn, set_digest: value.set_digest });
}

export function sameInstructionWatermark(a: InstructionWatermark, b: InstructionWatermark): boolean {
  return a.through_lsn === b.through_lsn && a.set_digest === b.set_digest;
}

export function decodeInstructionSummary(value: unknown): InstructionReviewSummary | null {
  if (!object(value) || typeof value.pending_comparison !== 'boolean'
    || !count(value.intervals) || !count(value.changed_paths) || !count(value.gaps)) return null;
  const watermark = decodeInstructionWatermark(value.watermark);
  return watermark === null ? null : Object.freeze({ watermark, pending_comparison: value.pending_comparison,
    intervals: value.intervals, changed_paths: value.changed_paths, gaps: value.gaps });
}

function context(value: unknown): InstructionContext | null {
  if (!object(value)) return null;
  if (value.kind === 'shared_project' || value.kind === 'unshared_project' || value.kind === 'unresolved') return Object.freeze({ kind: value.kind });
  if (value.kind === 'owned_worktree' && typeof value.lease === 'string' && ID.test(value.lease)) {
    return Object.freeze({ kind: value.kind, lease: value.lease });
  }
  if (value.kind === 'shared_input' && ['profile', 'ancestor', 'import', 'explicit_file'].includes(value.source as string)) {
    return Object.freeze({ kind: value.kind, source: value.source as 'profile' | 'ancestor' | 'import' | 'explicit_file' });
  }
  return null;
}

function path(value: unknown): InstructionPath | null {
  if (!object(value) || UTF8.encode(JSON.stringify(value)).length > 4_096) return null;
  if (value.encoding === 'utf8' && typeof value.value === 'string' && value.value !== '' && !value.value.includes('\0')) {
    return Object.freeze({ encoding: value.encoding, value: value.value });
  }
  if (value.encoding === 'unix_bytes' && typeof value.hex === 'string' && /^(?:[0-9a-f]{2})+$/.test(value.hex)
    && !value.hex.match(/../g)?.includes('00')) return Object.freeze({ encoding: value.encoding, hex: value.hex });
  if (value.encoding === 'windows_wide' && Array.isArray(value.units) && value.units.length > 0
    && value.units.every((unit) => count(unit) && unit > 0 && unit <= 65_535)) {
    return Object.freeze({ encoding: value.encoding, units: Object.freeze([...value.units]) as readonly number[] });
  }
  return null;
}

function interval(value: unknown): InstructionInterval | null {
  if (!object(value) || !['managed', 'attached_process', 'attached_turn'].includes(value.kind as string)
    || typeof value.agent !== 'string' || !ID.test(value.agent) || !nullableId(value.turn)
    || !nullableLsn(value.attachment_lsn) || !nullableLsn(value.issue_lsn) || !nullableLsn(value.baseline_lsn)
    || typeof value.missing_start !== 'boolean' || typeof value.card !== 'string' || !ID.test(value.card)
    || typeof value.goal !== 'string' || !ID.test(value.goal) || !lsn(value.assignment_lsn)
    || !(typeof value.seq_at_start === 'number' && Number.isSafeInteger(value.seq_at_start) && value.seq_at_start >= 0)
    || !nullableLsn(value.finished_lsn) || !nullableLsn(value.end_lsn)
    || !(value.sampled_at === null || (typeof value.sampled_at === 'number' && Number.isSafeInteger(value.sampled_at)))
    || typeof value.recovery !== 'boolean') return null;
  if (value.kind === 'managed' && value.missing_start
    && (value.turn === null || value.issue_lsn === null || value.baseline_lsn !== null
      || value.end_lsn !== null || value.sampled_at !== null || !value.recovery)) return null;
  return Object.freeze({ kind: value.kind as InstructionInterval['kind'], agent: value.agent, turn: value.turn,
    attachment_lsn: value.attachment_lsn, issue_lsn: value.issue_lsn, baseline_lsn: value.baseline_lsn,
    missing_start: value.missing_start, card: value.card, goal: value.goal, assignment_lsn: value.assignment_lsn, seq_at_start: value.seq_at_start,
    finished_lsn: value.finished_lsn, end_lsn: value.end_lsn, sampled_at: value.sampled_at, recovery: value.recovery });
}

export function decodeInstructionReview(value: unknown): InstructionReview | null {
  if (!object(value) || typeof value.pending_comparison !== 'boolean' || !Array.isArray(value.intervals)
    || !Array.isArray(value.changed_paths) || !Array.isArray(value.gaps)
    || UTF8.encode(JSON.stringify(value)).length > 65_536) return null;
  const watermark = decodeInstructionWatermark(value.watermark);
  if (watermark === null) return null;
  const intervals = value.intervals.map(interval);
  if (intervals.some((row) => row === null)) return null;
  const paths: InstructionShownPath[] = [];
  for (const row of value.changed_paths) {
    if (!object(row) || !display(row.display) || typeof row.display_truncated !== 'boolean') return null;
    const native = path(row.path);
    const scope = context(row.context);
    if (native === null || scope === null) return null;
    paths.push(Object.freeze({ path: native, display: row.display, display_truncated: row.display_truncated, context: scope }));
  }
  const gaps: InstructionGap[] = [];
  for (const row of value.gaps) {
    if (!object(row) || !(INSTRUCTION_GAPS as readonly unknown[]).includes(row.reason) || !nullableId(row.agent) || typeof row.display_truncated !== 'boolean') return null;
    const scope = context(row.context);
    const native = row.path === null ? null : path(row.path);
    if (scope === null || (row.path !== null && (native === null || !display(row.display)))
      || (row.path === null && row.display !== null)) return null;
    gaps.push(Object.freeze({ reason: row.reason as InstructionGapReason, context: scope, agent: row.agent,
      path: native, display: row.display as string | null, display_truncated: row.display_truncated }));
  }
  return Object.freeze({ watermark, pending_comparison: value.pending_comparison,
    intervals: Object.freeze(intervals as InstructionInterval[]), changed_paths: Object.freeze(paths), gaps: Object.freeze(gaps) });
}

export function instructionContextText(scope: InstructionContext): string {
  switch (scope.kind) {
    case 'owned_worktree': return `Owned worktree ${scope.lease}`;
    case 'shared_project': return 'Shared project';
    case 'unshared_project': return 'Unshared project';
    case 'shared_input': return `Shared ${scope.source.replaceAll('_', ' ')}`;
    case 'unresolved': return 'Unresolved context';
  }
}

const COVERAGE_ENGINES = Object.freeze({ codex: 'Codex', claude: 'Claude', pi: 'Pi' } as const);
const COVERAGE_PHRASES = Object.freeze({
  configuration_unknown: 'settings in its home folder', fallback_unknown: 'fallback file names',
  root_marker_unknown: 'root markers', plugin_unknown: 'plugins', policy_unknown: 'managed policy',
  path_unresolved: 'project folder lookup',
} as const);
const COVERAGE_OTHER = 'other inputs';

/** What Dudes could not see on a ticket that passed without a seen change. It holds nothing. */
export interface CoverageNote {
  readonly provider: string | null;
  readonly limits: readonly string[];
  readonly saw_no_change: boolean;
}

export function decodeCoverageNote(value: unknown): CoverageNote | null {
  if (!object(value) || !Array.isArray(value.limits) || value.limits.length > 8
    || !value.limits.every((limit) => typeof limit === 'string' && limit.length > 0 && limit.length <= 64)
    || new Set(value.limits).size !== value.limits.length || typeof value.saw_no_change !== 'boolean'
    || (value.limits.length === 0 && value.saw_no_change)) return null;
  const provider = value.provider ?? null;
  if (provider !== null && typeof provider !== 'string') return null;
  return Object.freeze({ provider, limits: Object.freeze([...value.limits]) as readonly string[], saw_no_change: value.saw_no_change });
}

export function coverageText(note: CoverageNote): string {
  const engine = note.provider !== null && Object.hasOwn(COVERAGE_ENGINES, note.provider)
    ? COVERAGE_ENGINES[note.provider as keyof typeof COVERAGE_ENGINES] : 'the engine';
  const phrases = [...new Set(note.limits.map((limit) => (Object.hasOwn(COVERAGE_PHRASES, limit)
    ? COVERAGE_PHRASES[limit as keyof typeof COVERAGE_PHRASES] : COVERAGE_OTHER)))];
  const last = phrases.pop() as string;
  const list = phrases.length === 0 ? last : `${phrases.join(', ')} or ${last}`;
  return `Dudes cannot see ${engine}'s ${list}.${note.saw_no_change ? ' It saw no change to the instruction and settings files it can read.' : ''}`;
}
