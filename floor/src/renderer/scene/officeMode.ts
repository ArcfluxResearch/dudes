// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The whole office's mode, from the control flags the Floor already reads on
// each row. Each frozen figure already says so on its own badge; this is the
// office saying it once.

import type { AgentRow } from './cast.ts';

export type OfficeMode = 'live' | 'paused' | 'halted';

/** The mode and the line the office shows for it. */
export interface OfficeModeView {
  readonly mode: OfficeMode;
  readonly line: string;
}

const PAUSED = 'The office is paused. No agent starts a new turn until it is resumed.';

export const LIVE_OFFICE: OfficeModeView = Object.freeze({ mode: 'live', line: '' });

/**
 * Halted when a halt stands on every current agent, paused when every one is
 * paused or halted, and live otherwise, including an empty office. `null`
 * while no confirmed snapshot backs the rows, as during the resync that ends
 * Pause all: those rows say nothing yet.
 */
export function officeModeOf(floor: {
  readonly phase: string;
  readonly epoch: number;
  readonly snapshotEpoch: number;
  readonly agents: readonly AgentRow[];
}): OfficeModeView | null {
  const { agents } = floor;
  if (floor.phase !== 'live' || floor.snapshotEpoch !== floor.epoch) return null;
  const halted = agents.filter((row) => row.halted === true).length;
  if (agents.length === 0) return LIVE_OFFICE;
  if (halted === agents.length) {
    return { mode: 'halted', line: 'The office is halted. A halt stands on every agent. Resume lifts a stop made from this page; '
      + 'only the local operator can lift one made from the command line.' };
  }
  if (!agents.every((row) => row.paused === true || row.halted === true)) return LIVE_OFFICE;
  return { mode: 'paused', line: halted === 0 ? PAUSED : `${PAUSED} Halted agents stay halted.` };
}

/**
 * What the office shows next, and what a screen reader hears, or `null` for
 * nothing new. `shown` stays `undefined` until the first confirmed live
 * snapshot, and the mode that snapshot brings is shown without a word. Rows no
 * snapshot confirms leave the office as it was while one is on its way, so a
 * resync neither blinks the line nor repeats it. A stream that gave up
 * confirms nothing, so the line goes, without a word. A return to live says
 * only what is true: some agent may run again, and an emptied office says
 * nothing.
 */
export function followMode(shown: OfficeModeView | undefined, confirmed: OfficeModeView | null,
  floor: { readonly phase: string; readonly agents: readonly unknown[] }): { readonly shown: OfficeModeView | undefined; readonly heard: string | null } {
  if (confirmed === null) return { shown: floor.phase === 'closed' && shown !== undefined ? LIVE_OFFICE : shown, heard: null };
  if (shown === undefined) return { shown: confirmed, heard: null };
  if (confirmed.mode === shown.mode && confirmed.line === shown.line) return { shown, heard: null };
  if (confirmed.mode !== 'live') return { shown: confirmed, heard: confirmed.line };
  return { shown: confirmed, heard: floor.agents.length === 0 ? null : `The office is no longer fully ${shown.mode}.` };
}
