// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Retire, its one confirmation, and the one request's status.
//
// Retire is drawn in the selected agent's detail and in its terminal view, and
// one confirmation serves both. The status lives in the receipts beside the drawer,
// because the agent's detail leaves with the agent.

import { UserMinus } from 'lucide-react';
import { useSyncExternalStore } from 'react';
import type { ReactNode, RefCallback } from 'react';

import { floorSessionGeneration } from '../../session.ts';
import { ICON } from '../../ui/cx.ts';
import { Button, Dialog, MenuItem } from '../../ui/index.ts';
import {
  blocksControls, browserControlDeps, controlGate, getControlState, submitControl, subscribeToControl,
} from '../control/control.ts';
import type { ControlState } from '../control/control.ts';
import type { FloorState } from '../floor/store.ts';
import { handFocusOnLeave } from '../focus.ts';
import { UnavailableControls } from '../settings/SettingsView.tsx';
import type { AgentRow } from '../scene/cast.ts';
import {
  afterRetireRefusal, blocksRetire, cancelRetire, checkRetireStatus, checkable, confirmRetire, getRetireState, dismissReceipt, receiptDismissible,
  leftTheFloor, retireOffer, submitRetire, subscribeToRetire,
} from './retire.ts';
import type { RetireAttempt, RetireRefusalReason, RetireState } from './retire.ts';

const HALTED_LINE = 'This agent is halted. Lift the halt from the command line before retiring it from this page.';

const CONSEQUENCES = 'Its current work stops now, and its terminal closes. It leaves the Floor for good. Only a new '
  + 'hire can replace it, and a new hire starts without its memory. Dudes keeps its history.';

const STATUS_TITLE = 'retire-status-title';

export function RetireStrip({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow | null }) {
  const retire = useSyncExternalStore(subscribeToRetire, getRetireState, getRetireState);
  return <RetireStripView retire={retire} floor={floor} row={row} session={floorSessionGeneration()} />;
}

/** Retire as an item of the agent's More menu. It opens the same confirmation as the strip's button. */
export function RetireMenuItem({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow | null }) {
  const retire = useSyncExternalStore(subscribeToRetire, getRetireState, getRetireState);
  const offer = retireOffer(floor, row);
  if (offer.kind === 'none') return null;
  if (offer.kind === 'halted') return <MenuItem danger disabled icon={<UserMinus {...ICON} />} title={HALTED_LINE}
    className="retire-open" onSelect={() => {}}>Retire agent</MenuItem>;
  const session = floorSessionGeneration();
  const waiting = blocksRetire(retire, offer, session);
  return <MenuItem danger icon={<UserMinus {...ICON} />} className="retire-open" data-retire-offer={offer.agent}
    aria-haspopup="dialog" aria-disabled={waiting || undefined}
    title={waiting ? 'Wait for the current retirement request or check its status.' : undefined}
    onSelect={() => confirmRetire(offer, session)}>Retire agent</MenuItem>;
}

export function RetireStripView({ retire, floor, row, session }: {
  readonly retire: RetireState;
  readonly floor: FloorState;
  readonly row: AgentRow | null;
  /** Which session the page holds now: `floorSessionGeneration()`. */
  readonly session: number;
}) {
  const offer = retireOffer(floor, row);
  if (offer.kind === 'none') return <UnavailableControls floor={floor} control="retire" />;
  if (offer.kind === 'halted') return <div className="retire-offer"><p className="hire-note">{HALTED_LINE}</p></div>;
  const pressedHere = retire.attempt?.payload.agent === offer.agent;
  return (
    <div className="retire-offer">
      {/* Waiting, never disabled: a disabled button drops the keyboard focus
          the closing confirmation hands back to it. */}
      <button ref={handFocusOnLeave} type="button" className="retire-open" data-retire-offer={offer.agent}
        data-focus-on-leave={pressedHere ? STATUS_TITLE : undefined} aria-haspopup="dialog"
        aria-disabled={blocksRetire(retire, offer, session) || undefined}
        title={blocksRetire(retire, offer, session) ? 'Wait for the current retirement request or check its status.' : undefined}
        onClick={() => confirmRetire(offer, session)}>Retire</button>
      {retire.problem === null || retire.attempt !== null ? null : (
        <p className="retire-problem" role="alert">{retire.problem}</p>
      )}
    </div>
  );
}

/**
 * A modal dialog keeps the rest of the page out of reach, and Escape fires
 * its `cancel`. Focus starts on Cancel, the choice that sends nothing, and
 * closing it hands focus back to the Retire that opened it.
 */
export const openConfirmation: RefCallback<HTMLDialogElement> = (node) => {
  if (node === null) return undefined;
  node.showModal();
  node.querySelector<HTMLElement>('.retire-cancel')?.focus();
  return () => {
    node.close();
  };
};

export function RetireDialog({ floor }: { readonly floor: FloorState }) {
  const retire = useSyncExternalStore(subscribeToRetire, getRetireState, getRetireState);
  const control = useSyncExternalStore(subscribeToControl, getControlState, getControlState);
  return <RetireDialogView retire={retire} control={control} floor={floor} session={floorSessionGeneration()} />;
}

export function RetireDialogView({ retire, control, floor, session }: {
  readonly retire: RetireState;
  readonly control: ControlState;
  readonly floor: FloorState;
  readonly session: number;
}) {
  const agent = retire.confirming;
  if (agent === null) return null;
  const row = floor.agents.find((candidate) => candidate.id === agent) ?? null;
  const offer = retireOffer(floor, row);
  if (offer.kind !== 'retire') return null;
  const pause = controlGate(floor, row);
  const pauseInstead = pause.open && pause.offer === 'pause' && !blocksControls(control, pause, session);
  return (
    <Dialog ref={openConfirmation} size="sm" className="retire-dialog" data-retire-dialog={offer.agent}
      aria-labelledby="retire-dialog-title" aria-describedby="retire-dialog-text" onCancel={cancelRetire}>
      <div className="grid gap-4 p-6">
        <h2 id="retire-dialog-title" className="m-0 font-sans text-title font-semibold text-ink">{`Retire ${offer.name}?`}</h2>
        <p id="retire-dialog-text" className="m-0 text-body text-muted">{CONSEQUENCES}</p>
        <div className="flex flex-wrap justify-end gap-2">
          <Button className="retire-cancel" onClick={cancelRetire}>Cancel</Button>
          {pauseInstead ? (
            <Button className="retire-pause"
              onClick={() => { cancelRetire(); void submitControl('pause', pause, browserControlDeps); }}>
              Pause instead
            </Button>
          ) : null}
          {/* The second click of a double-click on Retire outside would land here. */}
          <Button variant="danger" className="retire-confirm"
            onClick={(event) => { if (event.detail < 2) void submitRetire(offer, browserControlDeps); }}>Retire</Button>
        </div>
      </div>
    </Dialog>
  );
}

export function RetireStatus({ floor, leaveFocusTo = 'crew-toggle' }: { readonly floor: FloorState; readonly leaveFocusTo?: string }) {
  const retire = useSyncExternalStore(subscribeToRetire, getRetireState, getRetireState);
  return <RetireStatusView retire={retire} floor={floor} session={floorSessionGeneration()} leaveFocusTo={leaveFocusTo} />;
}

export function RetireStatusView({ retire, floor, session, leaveFocusTo = 'crew-toggle' }: {
  readonly retire: RetireState;
  readonly floor: FloorState;
  /** Which session the page holds now: `floorSessionGeneration()`. */
  readonly session: number;
  readonly leaveFocusTo?: string;
}) {
  const attempt = retire.attempt;
  if (attempt === null || retire.dismissedAttempt === attempt) return null;
  const outcome = retire.outcome;
  const asking = checkable(retire) || (retire.sending && outcome !== null);
  return (
    <section className="retire-status" aria-labelledby={STATUS_TITLE} aria-live="polite"
      data-retire-outcome={outcome?.kind ?? 'sending'} data-retire-agent-id={attempt.payload.agent}>
      <h3 id={STATUS_TITLE} tabIndex={-1}>
        <span className="agent-detail-kicker">Retire request</span> {attempt.name}
      </h3>
      <OutcomeBody attempt={attempt} retire={retire} floor={floor} session={session} />
      {retire.problem === null ? null : <p className="retire-problem" role="alert">{retire.problem}</p>}
      {asking ? (
        <footer className="retire-actions">
          <button type="button" aria-disabled={retire.sending || !floor.retire} aria-busy={retire.sending}
            title={retire.sending ? 'Wait for the current retirement request to finish.' : !floor.retire ? 'Reconnect to Dudes with Retire agents turned on in Settings.' : undefined}
            ref={handFocusOnLeave} data-focus-on-leave={STATUS_TITLE}
            onClick={() => { if (!retire.sending && floor.retire) void checkRetireStatus(browserControlDeps); }}>
            {retire.sending ? 'Checking…' : 'Check status'}
          </button>
        </footer>
      ) : null}
      {receiptDismissible(retire) ? <footer className="retire-actions">
        <button type="button" ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}
          onClick={dismissReceipt}>Dismiss receipt</button>
      </footer> : null}
      {!asking || retire.sending || floor.retire ? null : (
        <p className="hire-note">Check status waits for a live Floor session that offers Retire.</p>
      )}
    </section>
  );
}

const REFUSALS: Readonly<Record<RetireRefusalReason, ReactNode>> = Object.freeze({
  agent_absent: 'That agent is already retired.',
  agent_halted: HALTED_LINE,
  liveness_unknown: 'Dudes cannot yet tell whether this agent’s process is running. Try again in a moment.',
});

const ASK_AGAIN = 'Check status asks again with the same request, which never applies twice.';

function OutcomeBody({ attempt, retire, floor, session }: {
  readonly attempt: RetireAttempt;
  readonly retire: RetireState;
  readonly floor: FloorState;
  readonly session: number;
}) {
  const retiring = <p className="retire-pending" role="status">{`Retiring ${attempt.name}.`}</p>;
  const outcome = retire.outcome;
  if (outcome === null) return retiring;
  switch (outcome.kind) {
    case 'retired':
      return leftTheFloor(attempt, floor)
        ? <p className="retire-success" role="status">{`Retired ${attempt.name}.`}</p>
        : retiring;
    case 'pending':
      return <>{retiring}<p className="hire-note">{`Dudes has not confirmed this request yet. ${ASK_AGAIN}`}</p></>;
    case 'unavailable':
      return (
        <p className="retire-pending" role="status">
          Dudes could not confirm this request. Check status asks again with the same request; this page does
          not retry by itself.
        </p>
      );
    case 'unknown':
      return (
        <p className="retire-pending" role="status">
          {`The page lost Dudes’ reply, so it cannot say whether this request was recorded. ${ASK_AGAIN}`}
        </p>
      );
    case 'refused':
      return (
        <p className="retire-error" role="alert" data-retire-reason={outcome.reason}>{REFUSALS[outcome.reason]}</p>
      );
    case 'conflict':
      return (
        <p className="retire-error" role="alert">
          Dudes holds a different request under this request id, so it refused this one before any effect. This
          is a page error; nothing was recorded.
        </p>
      );
    case 'rejected':
      return (
        <p className="retire-error" role="alert">
          {`Dudes refused the request itself (HTTP ${outcome.status}) before any effect. Nothing was recorded.`}
        </p>
      );
    case 'forbidden': {
      const now = afterRetireRefusal(retire, floor, session);
      const next = retire.uncertain ? ASK_AGAIN : 'Nothing was retired.';
      return (
        <p className="retire-error" role="alert" data-retire-session={now}>
          {now === 'checking'
            ? 'Dudes refused this request before any effect. Checking whether this Floor session is still live…'
            : now === 'ended'
              ? 'Your Floor session has ended or was revoked. Open Dudes to sign in again '
                + `in this browser to act again. ${next}`
              : now === 'renewed'
                ? 'Dudes refused an earlier request before any effect, under a Floor session this page no '
                  + `longer uses. This page has signed in again. ${next}`
                : 'Dudes refused this request before any effect, and this page can no longer retire with its '
                  + `session. Open Dudes to sign in again, then act again. ${next}`}
        </p>
      );
    }
  }
}
