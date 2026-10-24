// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What an agent's newest ended turn reported about its cost and its context, in plain words.
// Display only: nothing here feeds a decision, and a number the engine did not report reads
// "not reported", never zero.

import { HIRE_ENGINES } from './onboarding/catalog.ts';
import type { AgentRow } from './scene/cast.ts';
import { formatUsd } from './scene/label.ts';

export interface Gauge {
  readonly used: number;
  readonly window: number;
  /** Whole percent, never above 100. */
  readonly percent: number;
  readonly text: string;
}

export interface LastTurnText {
  /** `12,300 tokens`, or why there is no figure. */
  readonly tokens: string;
  /** `1,000 new, 2,000 read from cache, 300 written to cache, 400 produced`, or `null` with no tokens. */
  readonly breakdown: string | null;
  readonly cost: string;
  readonly gauge: Gauge | null;
  /** Said when the gauge is missing or does not describe the next turn. */
  readonly contextNote: string;
  /** One line for the crew card, which labels it as the last turn. */
  readonly line: string;
}

function object(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null;
}

function count(value: unknown): number | null {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0 ? value : null;
}

const grouped = new Intl.NumberFormat('en-US');

function tokenCount(value: number): string {
  return grouped.format(value);
}

function compact(value: number): string {
  return value < 1000 ? String(value) : value < 1_000_000
    ? `${(value / 1000).toFixed(value < 10_000 ? 1 : 0).replace(/\.0$/, '')}k` : `${(value / 1_000_000).toFixed(1).replace(/\.0$/, '')}M`;
}

function windowName(value: number): string {
  return value >= 1_000_000 && value % 100_000 === 0 ? `${value / 1_000_000}M`
    : value >= 1000 && value % 1000 === 0 ? `${value / 1000}k` : tokenCount(value);
}

function engineName(provider: unknown): string {
  return typeof provider === 'string' ? (HIRE_ENGINES as Readonly<Record<string, string>>)[provider] ?? provider : 'the engine';
}

function model(value: unknown): string | null {
  return typeof value === 'string' && value !== '' ? value : null;
}

const ENGINE_OWN = 'the engine’s own model';

/** The last turn's report for a row, or `null` before any turn has ended. */
export function lastTurnOf(row: AgentRow): LastTurnText | null {
  const turn = object(row.last_turn);
  if (turn === null) return null;
  const engine = engineName(row.provider);
  const usage = object(turn.usage);
  const input = count(usage?.input);
  const output = count(usage?.output);
  const read = count(usage?.cache_read);
  const written = count(usage?.cache_creation);
  const known = usage !== null && input !== null && output !== null && read !== null && written !== null;
  const total = known ? input + output + read + written : null;
  const micros = count(usage?.usd);
  const cost = micros === null ? `Cost: not reported by ${engine}`
    : micros > 0 && micros < 10_000 ? 'Cost: under $0.01' : `Cost: ${formatUsd(micros)}`;
  const context = object(turn.context);
  const window = count(context?.window);
  const used = count(context?.used);
  const modelUsed = model(turn.model);
  const modelNext = model(row.model);
  const reported = model(context?.model);
  let gauge: Gauge | null = null;
  let contextNote: string;
  if (window === null || used === null || window === 0) {
    contextNote = `Context: not reported by ${engine}`;
  } else if (modelUsed !== modelNext) {
    contextNote = `The last turn filled ${tokenCount(used)} of a ${windowName(window)} window on ${reported ?? modelUsed ?? ENGINE_OWN}. `
      + `The next turn runs on ${modelNext ?? ENGINE_OWN}, whose window is not known until it runs.`;
  } else {
    const percent = Math.min(100, Math.floor((used / window) * 100));
    gauge = Object.freeze({ used, window, percent,
      text: `${percent}% of a ${windowName(window)} window (${tokenCount(used)} tokens), as of the last turn` });
    contextNote = '';
  }
  const parts = [total === null ? `tokens not reported by ${engine}` : `${compact(total)} tokens`,
    micros === null ? null : cost.replace('Cost: ', ''),
    gauge === null ? null : `${gauge.percent}% context`].filter((part): part is string => part !== null);
  return Object.freeze({
    tokens: total === null ? `Tokens: not reported by ${engine}` : `${tokenCount(total)} tokens`,
    breakdown: !known ? null
      : `${tokenCount(input)} new, ${tokenCount(read)} read from cache, ${tokenCount(written)} written to cache, ${tokenCount(output)} produced`,
    cost, gauge, contextNote,
    line: parts.join(' · '),
  });
}
