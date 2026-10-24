// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * The steps of a turn as the daemon sends them: what the agent ran, read and changed, already
 * scrubbed of secrets and cut to size. Everything here is text to show as written, never to
 * interpret, and nothing is trusted past its shape.
 */

export const STEP_KINDS = ['command', 'read', 'edit', 'write', 'search', 'web', 'plan', 'subagent', 'mcp', 'other'] as const;
export type StepKind = (typeof STEP_KINDS)[number];
export type StepStatus = 'running' | 'done' | 'failed';
export type PlanStatus = 'pending' | 'in_progress' | 'completed';

export interface StepArg { readonly key: string; readonly value: string; readonly cut: boolean }
export interface StepEdit { readonly old: string | null; readonly new: string; readonly cut: boolean }
export interface PlanItem { readonly text: string; readonly status: PlanStatus }
export interface StepItem {
  readonly id: string;
  readonly parent: string | null;
  readonly kind: StepKind;
  readonly tool: string;
  readonly status: StepStatus;
  readonly args: readonly StepArg[];
  readonly edit: StepEdit | null;
  readonly plan: readonly PlanItem[];
  readonly withheld: string | null;
  readonly exit: number | null;
  readonly resultBytes: number | null;
  readonly note: string | null;
  readonly startedAt: number;
  readonly endedAt: number | null;
}

export const EMPTY_STEPS: readonly StepItem[] = Object.freeze([]);
export const MAX_STEPS = 256;
export const MAX_STEPS_PER_FRAME = 512;

const STEP_ID = /^[A-Za-z0-9_-]{1,128}$/;
const MAX_TEXT = 8_192;
const MAX_ARGS = 16;
const MAX_PLAN = 64;
const STATUSES: readonly StepStatus[] = ['running', 'done', 'failed'];
const PLAN_STATUSES: readonly PlanStatus[] = ['pending', 'in_progress', 'completed'];

function record(value: unknown): Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) throw new TypeError('invalid step');
  return value as Record<string, unknown>;
}

function text(value: unknown, max = MAX_TEXT): string {
  if (typeof value !== 'string' || value.length > max) throw new TypeError('invalid step text');
  return value;
}

function optionalText(value: unknown): string | null {
  return value === undefined ? null : text(value);
}

function flag(value: unknown): boolean {
  if (value === undefined) return false;
  if (value !== true) throw new TypeError('invalid step flag');
  return true;
}

function integer(value: unknown, signed = false): number {
  if (typeof value !== 'number' || !Number.isSafeInteger(value) || (!signed && value < 0)) throw new TypeError('invalid step number');
  return value;
}

function optionalInteger(value: unknown, signed = false): number | null {
  return value === undefined ? null : integer(value, signed);
}

export function decodeStep(raw: unknown): StepItem {
  const step = record(raw);
  const id = text(step.id, 128);
  const parent = step.parent === undefined ? null : text(step.parent, 128);
  const kind = step.kind;
  const status = step.status;
  if (!STEP_ID.test(id) || (parent !== null && !STEP_ID.test(parent))
    || !(STEP_KINDS as readonly unknown[]).includes(kind)
    || !(STATUSES as readonly unknown[]).includes(status)) throw new TypeError('invalid step identity');
  const args = step.args === undefined ? [] : step.args;
  const plan = step.plan === undefined ? [] : step.plan;
  if (!Array.isArray(args) || args.length > MAX_ARGS || !Array.isArray(plan) || plan.length > MAX_PLAN) {
    throw new TypeError('invalid step lists');
  }
  let edit: StepEdit | null = null;
  if (step.edit !== undefined) {
    const raw = record(step.edit);
    edit = Object.freeze({ old: optionalText(raw.old), new: text(raw.new), cut: flag(raw.cut) });
  }
  return Object.freeze({
    id, parent, kind: kind as StepKind, tool: text(step.tool, 128), status: status as StepStatus,
    args: Object.freeze(args.map((arg) => {
      const item = record(arg);
      return Object.freeze({ key: text(item.key, 128), value: text(item.value), cut: flag(item.cut) });
    })),
    edit,
    plan: Object.freeze(plan.map((entry) => {
      const item = record(entry);
      if (!(PLAN_STATUSES as readonly unknown[]).includes(item.status)) throw new TypeError('invalid plan status');
      return Object.freeze({ text: text(item.text, 1_024), status: item.status as PlanStatus });
    })),
    withheld: optionalText(step.withheld),
    exit: optionalInteger(step.exit, true),
    resultBytes: optionalInteger(step.result_bytes),
    note: optionalText(step.note),
    startedAt: integer(step.started_at),
    endedAt: optionalInteger(step.ended_at),
  });
}

/** The list after a frame: a changed step keeps its place, a new one goes last, the oldest go first. */
export function mergeSteps(current: readonly StepItem[], changed: readonly StepItem[]): readonly StepItem[] {
  const next = [...current];
  for (const step of changed) {
    const at = next.findIndex((old) => old.id === step.id);
    if (at >= 0) next[at] = step;
    else next.push(step);
  }
  return Object.freeze(next.slice(Math.max(0, next.length - MAX_STEPS)));
}

export const LABEL_WORDS: Readonly<Record<StepKind, string>> = Object.freeze({
  command: 'Running a command',
  read: 'Reading a file',
  edit: 'Editing a file',
  write: 'Writing a file',
  search: 'Searching',
  web: 'Using the web',
  plan: 'Updating the plan',
  subagent: 'Working with a helper',
  mcp: 'Using a connected tool',
  other: 'Using a tool',
});

export const EMPTY_LABELS: Readonly<Record<string, string>> = Object.freeze({});
const MAX_LABELS = 128;

/** The words for the kind each running agent is in, from a `floor.step.labels` frame. */
export function decodeLabels(raw: unknown, isAgent: (id: unknown) => boolean): Readonly<Record<string, string>> {
  if (!Array.isArray(raw) || raw.length > MAX_LABELS) throw new TypeError('invalid step labels');
  const labels: Record<string, string> = {};
  for (const entry of raw) {
    const item = record(entry);
    if (!isAgent(item.agent_id) || !(STEP_KINDS as readonly unknown[]).includes(item.label)) {
      throw new TypeError('invalid step label');
    }
    labels[item.agent_id as string] = LABEL_WORDS[item.label as StepKind];
  }
  return Object.freeze(labels);
}

export function sameLabels(a: Readonly<Record<string, string>>, b: Readonly<Record<string, string>>): boolean {
  const keys = Object.keys(a);
  return keys.length === Object.keys(b).length && keys.every((key) => a[key] === b[key]);
}
