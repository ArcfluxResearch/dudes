// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What happened to a ticket and when: the newest events the daemon kept, as closed tokens, agent ids,
// rounds, priorities and exit codes. An event carries no title, note, message or path, so each is
// worded here from its kind and nothing in it is markup or a link.

import { TICKET_REASONS } from '../board.ts';
import type { TicketReason } from '../board.ts';
import { PRIORITY_WORDS, priorityOf } from './priority.ts';
import type { Priority } from './priority.ts';

const AGENT_ID = /^[0-9A-HJKMNP-TV-Z]{26}$/;
const LSN = /^(?:0|[1-9][0-9]{0,19})$/;
/** The daemon keeps 48 events a ticket, so a longer list is not one it sent. */
const MAX_EVENTS = 48;

export type HistoryBy = 'operator' | 'daemon' | 'agent';

export type HistoryKind =
  | { readonly kind: 'created' }
  | { readonly kind: 'edited' }
  | { readonly kind: 'assigned'; readonly agent: string }
  | { readonly kind: 'sent_back'; readonly agent: string }
  | { readonly kind: 'started'; readonly agent: string; readonly round: number }
  | { readonly kind: 'moved'; readonly reason: TicketReason }
  | { readonly kind: 'check_passed'; readonly exit: number | null }
  | { readonly kind: 'check_failed'; readonly exit: number | null }
  | { readonly kind: 'priority_set'; readonly priority: Priority }
  | { readonly kind: 'accepted' }
  | { readonly kind: 'returned'; readonly agent: string };

export type HistoryEvent = HistoryKind & {
  /** The journal position of the record that caused it, as decimal text. */
  readonly lsn: string;
  /** Unix milliseconds. */
  readonly at: number;
  readonly by: HistoryBy;
};

export interface TicketHistory {
  /** Oldest first, as the daemon sends them. */
  readonly events: readonly HistoryEvent[];
  /** How many earlier events the ticket had that are not in `events`. */
  readonly omitted: number;
}

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function count(value: unknown): value is number {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 && value <= 4_294_967_295;
}

function agentId(value: unknown): value is string {
  return typeof value === 'string' && AGENT_ID.test(value);
}

function exitOf(value: unknown): number | null | undefined {
  if (value === undefined || value === null) return null;
  return typeof value === 'number' && Number.isInteger(value) && value >= -2_147_483_648 && value <= 2_147_483_647
    ? value : undefined;
}

function eventOf(value: unknown): HistoryEvent | null {
  if (!record(value) || typeof value.lsn !== 'string' || !LSN.test(value.lsn)
    || typeof value.at !== 'number' || !Number.isSafeInteger(value.at) || Math.abs(value.at) > 8_640_000_000_000_000
    || (value.by !== 'operator' && value.by !== 'daemon' && value.by !== 'agent')) return null;
  const head: { readonly lsn: string; readonly at: number; readonly by: HistoryBy } = { lsn: value.lsn, at: value.at, by: value.by };
  const agent = agentId(value.agent) ? value.agent : null;
  switch (value.kind) {
    case 'created': return Object.freeze({ ...head, kind: 'created' });
    case 'edited': return Object.freeze({ ...head, kind: 'edited' });
    case 'accepted': return Object.freeze({ ...head, kind: 'accepted' });
    case 'assigned': return agent === null ? null : Object.freeze({ ...head, kind: 'assigned', agent });
    case 'sent_back': return agent === null ? null : Object.freeze({ ...head, kind: 'sent_back', agent });
    case 'returned': return agent === null ? null : Object.freeze({ ...head, kind: 'returned', agent });
    case 'started': return agent === null || !count(value.round) ? null
      : Object.freeze({ ...head, kind: 'started', agent, round: value.round });
    case 'moved': return typeof value.reason === 'string' && (TICKET_REASONS as readonly string[]).includes(value.reason)
      ? Object.freeze({ ...head, kind: 'moved', reason: value.reason as TicketReason }) : null;
    case 'check_passed': {
      const exit = exitOf(value.exit);
      return exit === undefined ? null : Object.freeze({ ...head, kind: 'check_passed', exit });
    }
    case 'check_failed': {
      const exit = exitOf(value.exit);
      return exit === undefined ? null : Object.freeze({ ...head, kind: 'check_failed', exit });
    }
    case 'priority_set': {
      const priority = priorityOf(value.priority);
      return priority === null ? null : Object.freeze({ ...head, kind: 'priority_set', priority });
    }
    default: return null;
  }
}

/**
 * The history in a ticket read: `undefined` when the daemon sent none (an older daemon), `null` when what it sent
 * is not the shape it uses, and the events otherwise.
 */
export function decodeHistory(events: unknown, omitted: unknown): TicketHistory | null | undefined {
  if (events === undefined) return undefined;
  const missed = omitted ?? 0;
  if (!Array.isArray(events) || events.length > MAX_EVENTS || !count(missed)) return null;
  const decoded = events.map(eventOf);
  if (decoded.some((event) => event === null)) return null;
  return Object.freeze({ events: Object.freeze(decoded as HistoryEvent[]), omitted: missed });
}

/** Why a ticket moved into Needs you, in a few words. */
const MOVED: Readonly<Record<TicketReason, string>> = Object.freeze({
  project_unavailable: 'its project is not set up any more',
  check_cannot_run: 'the completion check could not run',
  ceiling_required: 'a spending limit is needed',
  reserve_missing: 'a spending reserve is missing',
  goal_changed: "the agent's task was changed outside the board",
  limit_goal: 'it reached its spending limit',
  slowed: 'the agent kept failing and was slowed down',
  no_turns: 'it reached the work attempt limit',
  held_launch: "the agent's engine would not start",
  held_failing: 'the engine kept failing',
  held_unsure: 'Dudes could not confirm how the last work attempt ended',
  worktree_missing: "the agent's project copy is missing",
  shared_instructions_changed: 'shared project instructions changed',
  shared_instructions_unknown: 'Dudes could not tell whether shared project instructions changed',
  recovery_required: 'the work was interrupted',
});

const WHO = 'an agent';

/** One plain sentence for an event. `nameOf` gives an agent's name, or `null` for one that is gone. */
export function historySentence(event: HistoryEvent, nameOf: (agent: string) => string | null): string {
  const agent = 'agent' in event ? nameOf(event.agent) ?? WHO : WHO;
  const code = (exit: number | null) => exit === null ? '' : ` (exit ${exit})`;
  switch (event.kind) {
    case 'created': return 'Ticket created.';
    case 'edited': return 'Edited.';
    case 'assigned': return `Given to ${agent}.`;
    case 'sent_back': return `Sent back to ${agent}.`;
    case 'started': return `Round ${event.round} started with ${agent}.`;
    case 'moved': return `Moved to Needs you: ${MOVED[event.reason]}.`;
    case 'check_passed': return `Check passed${code(event.exit)}.`;
    case 'check_failed': return `Check failed${code(event.exit)}.`;
    case 'priority_set': return event.priority === 'none' ? 'Priority cleared.' : `Priority set to ${PRIORITY_WORDS[event.priority]}.`;
    case 'accepted': return 'Accepted.';
    case 'returned': return `Returned to To do when ${agent} was retired.`;
  }
}

/** "3 earlier events not shown." */
export function omittedSentence(omitted: number): string {
  return `${omitted} earlier event${omitted === 1 ? '' : 's'} not shown.`;
}
