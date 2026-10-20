// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';

import { floorSessionGeneration } from '../../session.ts';
import {
  REFUSAL_REASONS, blocksControls, browserControlDeps, getControlState, sentFrom, submitControl, subscribeToControl,
} from '../control/control.ts';
import type { CardRow } from '../board.ts';
import type { FloorState } from '../floor/store.ts';
import { stuckOffer } from './stuck.ts';

/** Resume, or Review recovery, beside the sentence that says the agent is held or interrupted. */
export function StuckActions({ floor, card, onReviewRecovery }: {
  readonly floor: FloorState;
  readonly card: CardRow;
  readonly onReviewRecovery?: (agent: string) => void;
}) {
  const control = useSyncExternalStore(subscribeToControl, getControlState, getControlState);
  const offer = stuckOffer(floor, card);
  if (offer.resume === null && offer.recovery === null && offer.switchOff === null) return null;
  const { attempt, outcome } = control;
  const mine = offer.resume !== null && attempt !== null && sentFrom(attempt, offer.resume);
  const blocked = offer.resume !== null && blocksControls(control, offer.resume, floorSessionGeneration());
  const sent = !mine ? null : control.sending ? 'Sending…'
    : outcome?.kind === 'applied' ? 'Resume recorded. This ticket changes when the agent starts its next turn.'
      : outcome?.kind === 'refused' ? REFUSAL_REASONS[outcome.reason] : null;
  return <div className="ticket-stuck" role="group" aria-label="Agent actions">
    {offer.resume === null ? null : <button type="button" className="ticket-action" data-action="resume"
      aria-disabled={blocked || undefined}
      onClick={(event) => { if (event.detail < 2) void submitControl('resume', offer.resume!, browserControlDeps); }}>
      Resume agent</button>}
    {offer.recovery === null ? null : <button type="button" className="ticket-action" data-action="review_recovery"
      onClick={() => onReviewRecovery?.(offer.recovery!)}>Review recovery</button>}
    {offer.switchOff === null ? null : <p className="ticket-hint" role="note">
      Turn on {offer.switchOff} under Floor switches in Settings to do this here.</p>}
    {sent === null ? null : <p className="ticket-hint" role="status">{sent}</p>}
  </div>;
}
