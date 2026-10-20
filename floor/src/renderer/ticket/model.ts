// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The board as the operator reads it: which column a ticket is in, what its card says, what the
// operator may do to it, and what the banner counts. All of it is read from the snapshot's cards
// and agents and nothing else, so a turn ending moves nothing here.

import type { CardRow } from '../board.ts';
import type { FloorState } from '../floor/store.ts';
import type { AgentRow } from '../scene/cast.ts';
import { decodeWorkspace } from '../workspace.ts';
import { NO_AGENT_YET, noAgentInProjectSentence, placeInLine, reasonSentence, waitSentence } from './words.ts';
import type { Wait } from './words.ts';

export type TicketColumn = 'todo' | 'working' | 'ready' | 'done';

export const TICKET_COLUMNS: readonly { readonly key: TicketColumn; readonly title: string }[] = Object.freeze([
  { key: 'todo', title: 'To do' }, { key: 'working', title: 'Working' },
  { key: 'ready', title: 'Ready for you' }, { key: 'done', title: 'Done' },
]);

/** How many finished tickets the Done column shows at first, and how many more each "Show more" adds. */
export const DONE_SHOWN = 20;

export function noProjects(floor: FloorState): boolean {
  return floor.settings !== null ? floor.settings.workspaces.length === 0
    : floor.checks !== null && floor.checks.workspaces.length === 0;
}

export function agentChoiceSentence(floor: FloorState, project: string): string {
  const label = floor.settings?.workspaces.find((row) => row.workspace_id === project)?.label ?? project;
  return floor.agents.length === 0 ? NO_AGENT_YET : noAgentInProjectSentence(label);
}

/** The column a ticket is in, or `null` for a ticket in the Needs you lane. */
export function columnOf(card: CardRow): TicketColumn | null {
  switch (card.status) {
    case 'todo':
    case 'claimed': return 'todo';
    case 'doing': return 'working';
    case 'review': return 'ready';
    case 'done': return 'done';
    case 'blocked': return null;
  }
}

export function tickets(cards: readonly CardRow[] | undefined): readonly CardRow[] {
  return (cards ?? []).filter((card) => card.ticket !== null);
}

/**
 * The column a card without a ticket is drawn in, or `null` when no column tells the truth about it.
 * Review and Blocked say nothing about the operator, so they stay out of Ready for you and Needs
 * you. A card marked done by an agent is that agent's word, so it stays out of Done.
 */
export function plainColumnOf(card: CardRow): TicketColumn | null {
  switch (card.status) {
    case 'todo':
    case 'claimed': return 'todo';
    case 'doing': return 'working';
    case 'done': return card.status_by.kind === 'agent' ? null : 'done';
    case 'review':
    case 'blocked': return null;
  }
}

/** Cards without a ticket that no column fits. */
export function otherCards(cards: readonly CardRow[]): readonly CardRow[] {
  return cards.filter((card) => card.ticket === null && plainColumnOf(card) === null);
}

export interface BoardColumns {
  readonly columns: Readonly<Record<TicketColumn, readonly CardRow[]>>;
  /** Tickets in Needs you. */
  readonly needsYou: readonly CardRow[];
  /** Done cards past the newest `doneShown`. */
  readonly doneHidden: number;
}

/**
 * The board's columns. A queue shows in the order the daemon serves it, and the newest Done cards
 * come first. `cards` arrive in id order, which is creation order.
 */
export function boardColumns(cards: readonly CardRow[], doneShown: number = DONE_SHOWN): BoardColumns {
  const columnFor = (card: CardRow) => (card.ticket === null ? plainColumnOf(card) : columnOf(card));
  const byColumn = (column: TicketColumn) => cards.filter((card) => columnFor(card) === column);
  const todo = [...byColumn('todo')].sort((a, b) => queueOrder(a) - queueOrder(b) || a.id.localeCompare(b.id));
  const done = [...byColumn('done')].sort((a, b) => b.id.localeCompare(a.id));
  return Object.freeze({
    columns: Object.freeze({
      todo: Object.freeze(todo), working: Object.freeze(byColumn('working')), ready: Object.freeze(byColumn('ready')),
      done: Object.freeze(done.slice(0, doneShown)),
    }),
    needsYou: Object.freeze(tickets(cards).filter((card) => card.status === 'blocked')),
    doneHidden: Math.max(0, done.length - doneShown),
  });
}

/** Unassigned tickets first, then queued ones by their place in line. */
function queueOrder(card: CardRow): number {
  return card.status === 'claimed' ? 1 + (card.ticket?.position ?? 0) : 0;
}

export type TicketAction = 'give' | 'edit' | 'remove' | 'accept' | 'changes' | 'try_again' | 'stop';

export const ACTION_LABEL: Readonly<Record<TicketAction, string>> = Object.freeze({
  give: 'Give to agent', edit: 'Edit', remove: 'Remove', accept: 'Accept', changes: 'Ask for changes',
  try_again: 'Try again', stop: 'Stop',
});

export function requiresInstructionReview(card: CardRow): boolean {
  return card.ticket?.instruction_review !== undefined
    || card.ticket?.reason === 'shared_instructions_changed' || card.ticket?.reason === 'shared_instructions_unknown';
}

/** Every state has an action, so none is a dead end. */
export function actionsOf(card: CardRow): readonly TicketAction[] {
  if (requiresInstructionReview(card)) return ['accept', 'changes', 'stop'];
  switch (card.status) {
    case 'todo':
    case 'claimed': return ['give', 'edit', 'remove'];
    case 'doing': return ['stop'];
    case 'review': return ['accept', 'changes', 'remove'];
    case 'blocked': return ['try_again', 'stop', 'remove'];
    case 'done': return ['remove'];
  }
}

/** Whether the view is live and matches the newest snapshot, so what it says can be acted on or announced. */
export function viewConfirmed(floor: FloorState): boolean {
  return floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch;
}

/**
 * What an agent that finished its turn says of itself while its ticket waits on you: the agent
 * reads "done" or "idle", but the work is not accepted. `null` when its status stands.
 */
export function ticketWord(cards: readonly CardRow[] | undefined, agent: string, status: string): string | null {
  if (status !== 'done' && status !== 'idle') return null;
  const held = tickets(cards).filter((card) => card.claimed_by === agent);
  if (held.some((card) => card.status === 'blocked')) return 'needs you';
  return held.some((card) => card.status === 'review') ? 'ready for you' : null;
}

/** The ticket that holds an agent's project, if one does: its title names the wait. */
export function holdingTicket(floor: FloorState, agent: string): CardRow | null {
  const project = decodeWorkspace(agentRow(floor, agent)?.workspace)?.id;
  if (project === undefined) return null;
  return tickets(floor.cards).find((card) => (card.status === 'doing' || card.status === 'review')
    && decodeWorkspace(agentRow(floor, card.claimed_by)?.workspace)?.id === project) ?? null;
}

export function agentRow(floor: FloorState, id: string | null): AgentRow | null {
  return id === null ? null : floor.agents.find((row) => row.id === id) ?? null;
}

/** The project a ticket runs in, as its agent's recorded workspace names it. */
export function projectOf(floor: FloorState, card: CardRow): string | null {
  return decodeWorkspace(agentRow(floor, card.claimed_by)?.workspace)?.id ?? null;
}

export function agentName(floor: FloorState, id: string | null): string {
  const row = agentRow(floor, id);
  return typeof row?.name === 'string' && row.name !== '' ? row.name : id ?? 'no agent';
}

/**
 * Why a ticket given to an agent has not started, read from the agents and cards in the snapshot,
 * in the order the daemon decides it: the agent first, then the project. The panel shows the
 * daemon's own words, which also cover the cases a snapshot cannot see.
 */
export function waitOf(floor: FloorState, card: CardRow): { readonly wait: Wait; readonly text: string } | null {
  const ticket = card.ticket;
  if (ticket === null) return null;
  const say = (wait: Wait, title = '') => Object.freeze({ wait, text: waitSentence(wait, agentName(floor, card.claimed_by), title) });
  if (card.status === 'todo') return say(ticket.stopping ? 'stopping' : 'not_given');
  if (card.status !== 'claimed') return null;
  const agent = agentRow(floor, card.claimed_by);
  if (agent !== null) {
    if (agent.paused === true || agent.halted === true || agent.automatic_hold !== undefined) return say('agent_paused');
    if (agent.mode === 'attached') return say('agent_attached');
    if (agent.status === 'running' || agent.status === 'waiting') return say('agent_busy');
  }
  const project = projectOf(floor, card);
  const holder = project === null ? undefined : tickets(floor.cards).find((other) => other.id !== card.id
    && projectOf(floor, other) === project && (other.status === 'doing' || other.status === 'review'));
  return holder === undefined ? null : say('ticket', holder.title);
}

export interface CardWords {
  /** "next in line", "2nd in line", or `null` when the ticket is not queued. */
  readonly place: string | null;
  /** The waiting note or, in Needs you, the sentence that names the next step. */
  readonly note: string | null;
  readonly agent: string | null;
}

/** What a ticket's card says beyond its title. */
export function cardWords(floor: FloorState, card: CardRow): CardWords {
  const ticket = card.ticket;
  const agent = card.claimed_by === null ? null : agentName(floor, card.claimed_by);
  if (ticket === null) return { place: null, note: null, agent };
  if (card.status === 'blocked') return { place: null, note: reasonSentence(ticket.reason), agent };
  const place = card.status === 'claimed' && ticket.position !== null ? placeInLine(ticket.position) : null;
  return { place, note: waitOf(floor, card)?.text ?? null, agent };
}

/** What the banner counts. A ticket is counted by the status the daemon wrote and by nothing an agent does. */
export interface BannerCounts {
  readonly ready: number;
  readonly needs: number;
  /** Working tickets whose agent has an open question. */
  readonly asking: number;
}

export function bannerCounts(floor: FloorState): BannerCounts {
  const all = tickets(floor.cards);
  return Object.freeze({
    ready: all.filter((card) => card.status === 'review').length,
    needs: all.filter((card) => card.status === 'blocked').length,
    asking: all.filter((card) => card.status === 'doing' && asks(agentRow(floor, card.claimed_by))).length,
  });
}

function asks(row: AgentRow | null): boolean {
  return row !== null && typeof row.question === 'string' && row.question !== '';
}

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

/** "2 tickets are ready for you. 1 needs you. 1 agent is asking.", or `null` when nothing waits. */
export function bannerText(counts: BannerCounts): string | null {
  const parts: string[] = [];
  if (counts.ready > 0) parts.push(`${plural(counts.ready, 'ticket is', 'tickets are')} ready for you.`);
  if (counts.needs > 0) parts.push(`${counts.needs} ${counts.needs === 1 ? 'needs' : 'need'} you.`);
  if (counts.asking > 0) parts.push(`${plural(counts.asking, 'agent is', 'agents are')} asking.`);
  return parts.length === 0 ? null : parts.join(' ');
}

/** The tickets that wait on the operator, with the status they arrived in. */
export function waitingTickets(cards: readonly CardRow[]): readonly CardRow[] {
  return tickets(cards).filter((card) => card.status === 'review' || card.status === 'blocked');
}

/**
 * Which tickets arrived in Ready for you or Needs you since `seen`, and the set to compare with
 * next time. The first reading seeds the set and announces nothing, so a reloaded page says
 * nothing about tickets that were already waiting.
 */
export function arrivals(seen: ReadonlySet<string> | null, cards: readonly CardRow[]): {
  readonly announce: readonly CardRow[];
  readonly next: ReadonlySet<string>;
} {
  const waiting = waitingTickets(cards);
  const next = new Set(waiting.map((card) => `${card.id}:${card.status}`));
  if (seen === null) return { announce: [], next };
  return { announce: waiting.filter((card) => !seen.has(`${card.id}:${card.status}`)), next };
}

export function announcement(card: CardRow): string {
  return card.status === 'review' ? `${card.title} is ready for you.` : `${card.title} needs you.`;
}
