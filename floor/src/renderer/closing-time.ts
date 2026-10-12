// SPDX-License-Identifier: MIT OR Apache-2.0

export type ClosingTime = 'announced' | 'complete' | null;

export interface ClosingProgress {
  readonly acknowledged: number;
  readonly settled: number;
  readonly owed: number;
  readonly running: number;
  readonly remaining_ms: number;
}

export interface ClosingDisplay {
  readonly phase: Exclude<ClosingTime, null>;
  readonly progress: ClosingProgress | null;
}

export function decodeClosingProgress(value: unknown): { readonly value: ClosingProgress | null; readonly rejected: 0 | 1 } {
  if (value === undefined) return { value: null, rejected: 0 };
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return { value: null, rejected: 1 };
  const fields = value as Record<string, unknown>;
  const { acknowledged, settled, owed, running, remaining_ms } = fields;
  if (![acknowledged, settled, owed, running].every(count => typeof count === 'number' && Number.isInteger(count) && count >= 0 && count <= 4096)
    || typeof remaining_ms !== 'number' || !Number.isInteger(remaining_ms) || remaining_ms < 0 || remaining_ms > 360_000) return { value: null, rejected: 1 };
  const progress = { acknowledged, settled, owed, running, remaining_ms } as ClosingProgress;
  const total = progress.acknowledged + progress.settled + progress.owed;
  if (total > 4096 || progress.running > total) return { value: null, rejected: 1 };
  return { value: Object.freeze(progress), rejected: 0 };
}

const AGENT_ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;

export function decodeClosingTime(value: unknown): { readonly value: ClosingTime; readonly rejected: 0 | 1 } {
  if (value === undefined) return { value: null, rejected: 0 };
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return { value: null, rejected: 1 };
  const phase = value as Record<string, unknown>;
  if (phase.kind === 'announced' || phase.kind === 'complete') return { value: phase.kind, rejected: 0 };
  if (phase.kind === 'ack' && typeof phase.agent === 'string' && phase.agent.length === 26 && AGENT_ID.test(phase.agent)) {
    return { value: 'announced', rejected: 0 };
  }
  return { value: null, rejected: 1 };
}
