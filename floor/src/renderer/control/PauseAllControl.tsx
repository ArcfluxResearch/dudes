// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useState, useSyncExternalStore } from 'react';

import { floorSessionGeneration } from '../../session.ts';
import type { FloorState } from '../floor/store.ts';
import { handFocusOnLeave, restoreFocusAfterDialog } from '../focus.ts';
import { REFUSAL_REASONS, browserControlDeps } from './control.ts';
import {
  beginPauseAll, checkPauseAllStatus, dismissPauseAll, getPauseAllState, pauseAllCount, pauseAllTargets,
  subscribeToPauseAll,
} from './pauseAll.ts';
import type { PauseAllState } from './pauseAll.ts';

interface Confirmation {
  readonly targets: string;
  readonly session: number;
}

/** Where Confirm sends the keyboard: the result's own status line, once it renders. */
const RECEIPT_STATUS_ID = 'pause-all-receipt-status';

export function confirmationIsCurrent(confirmation: Confirmation | null, floor: FloorState, session: number): boolean {
  return confirmation !== null && confirmation.session === session && confirmation.targets === pauseAllTargets(floor);
}

export function PauseAllControl({ floor }: { readonly floor: FloorState }) {
  const state = useSyncExternalStore(subscribeToPauseAll, getPauseAllState, getPauseAllState);
  const [confirmation, setConfirmation] = useState<Confirmation | null>(null);
  const currentConfirmation = confirmationIsCurrent(confirmation, floor, floorSessionGeneration()) ? confirmation : null;
  const notice = confirmation !== null && currentConfirmation === null
    ? 'The Floor session or the agents to pause changed before confirmation. Open Pause all again.' : null;
  useEffect(() => {
    if (currentConfirmation !== null) document.getElementById('pause-all-cancel')?.focus();
  }, [currentConfirmation]);
  const cancel = () => {
    setConfirmation(null);
    document.getElementById('pause-all-open')?.focus();
  };
  return <PauseAllControlView floor={floor}
    state={state} confirming={currentConfirmation !== null} notice={notice}
    onOpen={() => {
      const targets = pauseAllTargets(floor);
      if (targets !== null) setConfirmation({ targets, session: floorSessionGeneration() });
    }} onCancel={cancel}
    onConfirm={() => {
      if (currentConfirmation === null) return;
      setConfirmation(null);
      void beginPauseAll(floor, browserControlDeps, currentConfirmation.session, currentConfirmation.targets);
      // The receipt mounts in the same commit, beside the office rather than here, so the DOM needs a frame
      // to settle before it can take the keyboard. beginPauseAll has its own early returns (a stale
      // confirmation, no targets left, or the shared control slot already held by a Pause or Resume) that
      // leave the pause idle and mount no receipt at all, so this falls back to the button Confirm came
      // from rather than losing focus to the page body.
      requestAnimationFrame(() => {
        restoreFocusAfterDialog(document.getElementById(RECEIPT_STATUS_ID), document.getElementById('pause-all-open'), (run) => run());
      });
    }} />;
}

/** The result of Pause all, kept with the other action receipts beside the office rather than over anything. */
export function PauseAllReceipt({ leaveFocusTo = 'crew-toggle' }: { readonly leaveFocusTo?: string }) {
  const state = useSyncExternalStore(subscribeToPauseAll, getPauseAllState, getPauseAllState);
  return <PauseAllReceiptView state={state} onCheck={() => { void checkPauseAllStatus(browserControlDeps); }}
    onDismiss={dismissPauseAll} leaveFocusTo={leaveFocusTo} />;
}

export function PauseAllControlView({ floor, currentFloor = floor, state, confirming,
  notice = null, onOpen, onCancel, onConfirm }: {
  readonly floor: FloorState;
  readonly currentFloor?: FloorState;
  readonly state: PauseAllState;
  readonly confirming: boolean;
  readonly notice?: string | null;
  readonly onOpen: () => void;
  readonly onCancel: () => void;
  readonly onConfirm: () => void;
}) {
  const count = pauseAllCount(floor);
  const available = currentFloor.control && currentFloor.phase === 'live' && !currentFloor.stale
    && currentFloor.snapshotEpoch === currentFloor.epoch;
  const problem = state.phase === 'idle' ? state.problem : null;
  if (!available && problem === null && notice === null) return null;
  const active = state.phase === 'sending' || state.phase === 'unresolved';
  return <div className="pause-all-control">
    {available ? <button type="button" className="pause-all-open" id="pause-all-open" aria-expanded={confirming}
      aria-controls={confirming ? 'pause-all-confirm' : undefined} disabled={active || count === 0}
      onClick={onOpen}>Pause all</button> : null}
    {confirming && available ? <section className="pause-all-confirm" id="pause-all-confirm" aria-label="Confirm Pause all">
      <p>Pause {count} current {count === 1 ? 'agent' : 'agents'}? Running turns continue to their end.</p>
      <div className="pause-all-actions">
        <button type="button" id="pause-all-cancel" onClick={onCancel}>Cancel</button>
        <button type="button" className="pause-all-submit" onClick={onConfirm}>Confirm pause all</button>
      </div>
    </section> : null}
    {notice !== null ? <p role="alert">{notice}</p> : problem !== null ? <p role="alert">{problem}</p> : null}
  </div>;
}

export function PauseAllReceiptView({ state, onCheck, onDismiss, leaveFocusTo = 'crew-toggle' }: {
  readonly state: PauseAllState;
  readonly onCheck: () => void;
  readonly onDismiss: () => void;
  readonly leaveFocusTo?: string;
}) {
  return state.phase === 'idle' ? null : <section className="pause-all-receipt" aria-live="polite" aria-label="Pause all result">
      <p id={RECEIPT_STATUS_ID} tabIndex={-1}>{state.phase === 'sending' ? `Pausing agents… ${state.paused} of ${state.total} paused.`
        : `Paused ${state.paused} ${state.paused === 1 ? 'agent' : 'agents'}.`}</p>
      {state.refusals.length > 0 ? <ul aria-label="Pause refusals">{state.refusals.map((refusal, at) =>
        <li key={`${refusal.name}-${at}`}>Refused {refusal.name}: {REFUSAL_REASONS[refusal.reason as keyof typeof REFUSAL_REASONS]
          ?? refusal.reason}</li>)}</ul> : null}
      {state.phase === 'complete' && state.problem !== null ? <p role="alert">{state.problem}</p> : null}
      {state.phase === 'unresolved' ? <>
        <p role="status">The pause for {state.remaining[0]?.name} is unresolved. Check its status before continuing.</p>
        {state.problem === null ? null : <p role="alert">{state.problem}</p>}
        <button type="button" onClick={onCheck}>Check status</button>
      </> : null}
      {state.phase === 'complete' ? <button type="button" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}
        onClick={onDismiss}>Dismiss receipt</button> : null}
    </section>;
}
