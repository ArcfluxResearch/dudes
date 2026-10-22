// SPDX-License-Identifier: MIT OR Apache-2.0

import type { FloorState } from '../floor/store.ts';
import { getFloorState } from '../floor/store.ts';
import { mintAttemptId } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';
import {
  controlGate, postControl, releasePauseAllSlot, reservePauseAllSlot,
} from './control.ts';
import type { ControlAttempt, ControlDeps, ControlOutcome } from './control.ts';

interface Target {
  readonly agent: string;
  readonly name: string;
  readonly controlLsn: string | null;
  readonly turns: number;
}

export interface PauseAllRefusal {
  readonly name: string;
  readonly reason: string;
}

export interface PauseAllState {
  readonly phase: 'idle' | 'sending' | 'unresolved' | 'complete';
  readonly total: number;
  readonly paused: number;
  readonly refusals: readonly PauseAllRefusal[];
  readonly remaining: readonly Target[];
  readonly current: ControlAttempt | null;
  readonly outcome: ControlOutcome | null;
  readonly uncertain: boolean;
  readonly problem: string | null;
  readonly session: number;
  readonly epoch: number;
}

const INITIAL: PauseAllState = Object.freeze({
  phase: 'idle', total: 0, paused: 0, refusals: Object.freeze([]), remaining: Object.freeze([]),
  current: null, outcome: null, uncertain: false, problem: null,
  session: 0, epoch: 0,
});

let state: PauseAllState = INITIAL;
const listeners = new Set<() => void>();

export function getPauseAllState(): PauseAllState { return state; }

export function subscribeToPauseAll(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

function commit(next: PauseAllState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

export function resetPauseAll(): void {
  releasePauseAllSlot();
  state = INITIAL;
  listeners.clear();
}

function nameOf(row: AgentRow): string {
  return typeof row.name === 'string' && row.name !== '' ? row.name : row.id;
}

export function pauseAllCount(floor: FloorState): number {
  if (!floor.control || floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return 0;
  return floor.agents.filter((row) => row.paused !== true && row.halted !== true).length;
}

/**
 * The agents Pause all would address, each with the control position its request would carry.
 * A confirmation stands while this is unchanged, so an unrelated delta leaves it open.
 */
export function pauseAllTargets(floor: FloorState): string | null {
  if (pauseAllCount(floor) === 0) return null;
  return JSON.stringify(floor.agents.filter((row) => row.paused !== true && row.halted !== true)
    .map((row) => [row.id, row.control_lsn ?? null]));
}

function targetsOf(floor: FloorState): { targets: Target[]; refusals: PauseAllRefusal[] } {
  const targets: Target[] = [];
  const refusals: PauseAllRefusal[] = [];
  for (const row of floor.agents) {
    if (row.paused === true || row.halted === true) continue;
    const gate = controlGate(floor, row);
    if (!gate.open) {
      refusals.push({ name: nameOf(row), reason: 'unreadable_control' });
      continue;
    }
    targets.push({ agent: gate.agent, name: nameOf(row), controlLsn: gate.controlLsn, turns: gate.turns });
  }
  return { targets, refusals };
}

function attemptFor(target: Target, deps: ControlDeps): ControlAttempt {
  const payload = Object.freeze({
    request_id: mintAttemptId(deps.random), agent: target.agent, control_lsn: target.controlLsn,
  });
  return Object.freeze({ verb: 'pause', payload, body: JSON.stringify(payload), turnsAtSend: target.turns });
}

export async function beginPauseAll(floor: FloorState, deps: ControlDeps,
  confirmedSession = deps.session(), confirmedTargets = pauseAllTargets(floor)): Promise<void> {
  if (state.phase === 'sending' || state.phase === 'unresolved') return;
  const current = getFloorState();
  if (pauseAllTargets(current) !== confirmedTargets || deps.session() !== confirmedSession
    || deps.epoch() !== current.epoch) {
    commit({ ...state, problem: 'The Floor session or the agents to pause changed before confirmation. Open Pause all again.' });
    return;
  }
  if (confirmedTargets === null) return;
  if (!reservePauseAllSlot()) {
    commit({ ...state, problem: 'Finish the earlier pause or resume before pausing all agents.' });
    return;
  }
  const { targets, refusals } = targetsOf(current);
  commit({ phase: 'sending', total: targets.length + refusals.length, paused: 0, refusals,
    remaining: targets, current: null, outcome: null, uncertain: false, problem: null,
    session: confirmedSession, epoch: current.epoch });
  await drain(deps);
}

export async function checkPauseAllStatus(deps: ControlDeps): Promise<void> {
  if (state.phase !== 'unresolved' || state.current === null) return;
  commit({ ...state, phase: 'sending' });
  await drain(deps);
}

async function drain(deps: ControlDeps): Promise<void> {
  while (state.remaining.length > 0) {
    // A same-id status check is permitted after renewal. It does not admit new requests
    // from the roster confirmed under the old session.
    if (state.current === null) {
      const floor = getFloorState();
      if (deps.session() !== state.session || deps.epoch() !== state.epoch || floor.phase !== 'live'
        || floor.stale || floor.snapshotEpoch !== floor.epoch || !floor.control) {
        commit({ ...state, phase: 'complete', problem: 'The Floor session or snapshot changed. Remaining agents were not sent.' });
        releasePauseAllSlot();
        return;
      }
    }
    const target = state.remaining[0];
    if (!deps.signedIn()) {
      if (state.current === null) {
        commit({ ...state, phase: 'complete', problem: 'This page has no Floor session. No further agents were sent.' });
        releasePauseAllSlot();
      } else {
        commit({ ...state, phase: 'unresolved', problem: 'Sign in to check this pause request.' });
      }
      return;
    }
    const attempt = state.current ?? attemptFor(target, deps);
    commit({ ...state, phase: 'sending', current: attempt, outcome: null, problem: null });
    const outcome = await postControl(attempt, deps.fetch);
    if (state.current !== attempt) return;
    if (outcome.kind === 'applied') {
      commit({ ...state, paused: state.paused + 1, remaining: state.remaining.slice(1),
        current: null, uncertain: false });
      continue;
    }
    if (outcome.kind === 'refused' || (!state.uncertain
      && (outcome.kind === 'conflict' || outcome.kind === 'rejected'))) {
      const reason = outcome.kind === 'refused' ? outcome.reason : outcome.kind;
      commit({ ...state, refusals: [...state.refusals, { name: target.name, reason }],
        remaining: state.remaining.slice(1), current: null, outcome, uncertain: false });
      continue;
    }
    if (outcome.kind === 'forbidden' && !state.uncertain) {
      commit({ ...state, phase: 'complete', current: null, outcome,
        refusals: [...state.refusals, { name: target.name, reason: 'forbidden' }],
        remaining: state.remaining.slice(1), problem: 'The Floor session was refused. No further agents were sent.' });
      releasePauseAllSlot();
      deps.refresh();
      return;
    }
    const uncertain = state.uncertain || outcome.kind === 'pending' || outcome.kind === 'unavailable'
      || outcome.kind === 'unknown';
    commit({ ...state, phase: 'unresolved', outcome, uncertain });
    if (outcome.kind === 'forbidden') deps.refresh();
    return;
  }
  commit({ ...state, phase: 'complete', current: null, outcome: null });
  releasePauseAllSlot();
  deps.refresh();
}

export function dismissPauseAll(): void {
  if (state.phase === 'complete') commit(INITIAL);
}
