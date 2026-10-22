// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What a ticket in Needs you can offer right where it says the agent is held or interrupted: a
// Resume, or a way into the recovery view in the agent drawer. A switch that is off gets a
// sentence naming it in Settings.

import { controlGate } from '../control/control.ts';
import type { ControlGate } from '../control/control.ts';
import type { CardRow, TicketReason } from '../board.ts';
import type { FloorState } from '../floor/store.ts';
import { recoveryEnabled } from '../recovery/recovery.ts';

const RESUME_REASONS: ReadonlySet<TicketReason | null> = new Set<TicketReason>([
  'held_launch', 'held_failing', 'held_unsure', 'worktree_missing',
]);

export type OpenResumeGate = Extract<ControlGate, { readonly open: true }> & { readonly offer: 'resume' };

export interface StuckOffer {
  /** Resume is possible now, through the control route. */
  readonly resume: OpenResumeGate | null;
  /** The agent whose recovery view the ticket can open. */
  readonly recovery: string | null;
  /** Which Floor switch to turn on in Settings before the button can appear, or `null`. */
  readonly switchOff: string | null;
}

const NONE: StuckOffer = Object.freeze({ resume: null, recovery: null, switchOff: null });

/** The Settings label of the switch behind Resume. */
export const RESUME_SWITCH = 'Pause, resume and rename';
/** The Settings label of the switch behind recovery. */
export const RECOVERY_SWITCH = 'Recover interrupted goals';

export function stuckOffer(floor: FloorState, card: CardRow): StuckOffer {
  const reason = card.ticket?.reason ?? null;
  if (card.status !== 'blocked' || card.claimed_by === null) return NONE;
  const row = floor.agents.find((agent) => agent.id === card.claimed_by) ?? null;
  if (row === null) return NONE;
  if (reason === 'recovery_required') {
    if (recoveryEnabled(floor, row)) return Object.freeze({ ...NONE, recovery: row.id });
    const off = [!floor.control ? RESUME_SWITCH : null, !floor.recovery ? RECOVERY_SWITCH : null].filter((x) => x !== null);
    return floor.settings && off.length > 0 ? Object.freeze({ ...NONE, switchOff: off.join(' and ') }) : NONE;
  }
  if (!RESUME_REASONS.has(reason)) return NONE;
  if (!floor.control) return floor.settings ? Object.freeze({ ...NONE, switchOff: RESUME_SWITCH }) : NONE;
  const gate = controlGate(floor, row);
  return gate.open && gate.offer === 'resume' ? Object.freeze({ ...NONE, resume: gate as OpenResumeGate }) : NONE;
}
