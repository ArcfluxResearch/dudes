// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Which daemon fact puts an agent where in the office, in what pose, under what badge.
//
// One reading of the row picks one office state, and one table maps that
// state to a place, a pose and a badge. Only facts the Floor already reads
// take part: the control flags, the status, an automatic hold, the goal's own
// status and a fresh live activity. A recent activity only refines a running
// or idle agent; a journaled status such as `done` or `waiting` outranks it.

import { automaticHoldOf } from '../automaticHold.ts';
import { liveActivityKind } from '../liveActivity.ts';
import type { LiveActivityContext } from '../liveActivity.ts';
import type { AgentRow } from './cast.ts';
import { fieldsOf } from './label.ts';

export type OfficeState = 'halted' | 'paused' | 'attached' | 'question' | 'held' | 'exhausted' | 'done'
  | 'compacting' | 'working' | 'between-turns' | 'idle';

/** `stay` keeps the agent where it is standing now. */
export type Place = 'desk' | 'board' | 'lounge' | 'kitchen' | 'bell' | 'stay';

/** `visit` is never a station's own pose: `visits.ts` paints it over whatever station an agent holds while it
 * walks to a colleague's desk, talks, and walks back. */
export type Pose = 'type' | 'point' | 'raise' | 'sit' | 'ring' | 'ponder' | 'wander' | 'still' | 'visit';

export type Badge = 'working' | 'checking' | 'question' | 'held' | 'exhausted' | 'done' | 'attached'
  | 'paused' | 'halted' | 'review';

export interface Station {
  readonly place: Place;
  readonly pose: Pose;
  readonly badge: Badge | null;
}

export const STATIONS: Readonly<Record<OfficeState, Station>> = Object.freeze({
  halted: { place: 'stay', pose: 'still', badge: 'halted' },
  paused: { place: 'stay', pose: 'still', badge: 'paused' },
  attached: { place: 'desk', pose: 'type', badge: 'attached' },
  question: { place: 'lounge', pose: 'raise', badge: 'question' },
  held: { place: 'lounge', pose: 'sit', badge: 'held' },
  exhausted: { place: 'lounge', pose: 'sit', badge: 'exhausted' },
  done: { place: 'bell', pose: 'ring', badge: 'done' },
  compacting: { place: 'kitchen', pose: 'ponder', badge: 'working' },
  working: { place: 'desk', pose: 'type', badge: 'working' },
  'between-turns': { place: 'board', pose: 'point', badge: 'checking' },
  idle: { place: 'desk', pose: 'wander', badge: null },
});

function openGoal(row: AgentRow): boolean {
  const goal = row.goal;
  if (typeof goal !== 'object' || goal === null) return false;
  const status = (goal as Record<string, unknown>).status;
  return status === 'set' || status === 'in_progress';
}

interface Reading {
  readonly state: OfficeState;
  /** Whether a live activity, rather than a journaled fact, chose the state. */
  readonly fromActivity: boolean;
}

function read(row: AgentRow, context: LiveActivityContext | undefined): Reading {
  const fact = (state: OfficeState): Reading => ({ state, fromActivity: false });
  if (row.halted === true) return fact('halted');
  if (row.paused === true) return fact('paused');
  const status = row.status;
  if (status === 'attached') return fact('attached');
  if (status === 'waiting') return fact('question');
  if (automaticHoldOf(row) !== null) return fact('held');
  if (status === 'exhausted') return fact('exhausted');
  if (status === 'done') return fact('done');
  const kind = context === undefined ? null : liveActivityKind(row, context);
  const activity = kind === 'working' || kind === 'using_tool' ? 'working'
    : kind === 'compacting' ? 'compacting' : kind === 'waiting' ? 'question' : null;
  if (activity !== null) return { state: activity, fromActivity: true };
  if (status === 'running') return fact('working');
  return fact(openGoal(row) ? 'between-turns' : 'idle');
}

/** The one office state a row is in. `context` is absent where no live activity may be read. */
export function officeStateOf(row: AgentRow, context?: LiveActivityContext): OfficeState {
  return read(row, context).state;
}

export function stationOf(row: AgentRow, context?: LiveActivityContext): Station & { readonly fromActivity: boolean } {
  const { state, fromActivity } = read(row, context);
  return { ...STATIONS[state], fromActivity };
}

/**
 * Whether the office shows a speech mark over this agent that opens its
 * question: only while the agent waits on a question the journal holds open
 * and its badge says so. A live activity alone raises the badge but has no
 * question to open, and a paused or halted agent wears that badge instead.
 */
export function asksOpenQuestion(row: AgentRow, context?: LiveActivityContext): boolean {
  return officeStateOf(row, context) === 'question' && fieldsOf(row).question !== '';
}
