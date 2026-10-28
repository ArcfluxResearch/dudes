// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Reading the board as a list: which tickets match the search and filters, the order the queue runs
// them in, and how a list is grouped. All of it comes from the snapshot's cards and agents. A card
// does not carry its project, so a ticket's project is its agent's, and the project of a ticket no
// agent holds is the one read from the ticket (see `placed.ts`).

import type { CardRow } from '../board.ts';
import type { FloorState } from '../floor/store.ts';
import { decodeWorkspace } from '../workspace.ts';
import { agentName, agentRow, boardColumns, columnOf, DONE_SHOWN, otherCards, plainColumnOf } from './model.ts';
import type { TicketColumn } from './model.ts';

export type BoardLayout = 'board' | 'list';
export type GroupBy = 'none' | 'project' | 'agent' | 'status';
/** The five places a ticket can be: the four columns and the Needs you lane. */
export type StatusKey = 'needs' | TicketColumn;

/** The filter value for tickets with no project or no agent. It cannot be a project id, which has no `~`. */
export const NONE = '~none';

export interface BrowseView {
  readonly layout: BoardLayout;
  readonly group: GroupBy;
  readonly project: string;
  readonly agent: string;
  readonly status: StatusKey | '';
  readonly search: string;
}

export const DEFAULT_VIEW: BrowseView = Object.freeze({
  layout: 'board', group: 'none', project: '', agent: '', status: '', search: '',
});

export const STATUS_NAMES: Readonly<Record<StatusKey, string>> = Object.freeze({
  needs: 'Needs you', ready: 'Ready for you', working: 'Working', todo: 'To do', done: 'Done',
});

/** The order the queue runs in: what waits on the operator, then what is under way, then what is next. */
const STATUS_ORDER: readonly StatusKey[] = Object.freeze(['needs', 'ready', 'working', 'todo', 'done']);

const PROJECT_ID = /^[a-zA-Z0-9_-]{1,128}$/;
const AGENT_ID = /^[0-9A-HJKMNP-TV-Z]{26}$/;
const MAX_STORED = 2048;

/** What the browser remembers: the layout, the grouping and the filters. A search is not remembered. */
export function parseRemembered(raw: string | null): BrowseView {
  if (raw === null || raw.length > MAX_STORED) return DEFAULT_VIEW;
  let value: unknown;
  try { value = JSON.parse(raw); } catch { return DEFAULT_VIEW; }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return DEFAULT_VIEW;
  const row = value as Record<string, unknown>;
  const text = (field: unknown, allowed: (candidate: string) => boolean, fallback: string): string =>
    typeof field === 'string' && allowed(field) ? field : fallback;
  return Object.freeze({
    layout: text(row.layout, (v) => v === 'board' || v === 'list', DEFAULT_VIEW.layout) as BoardLayout,
    group: text(row.group, (v) => v === 'none' || v === 'project' || v === 'agent' || v === 'status', DEFAULT_VIEW.group) as GroupBy,
    project: text(row.project, (v) => v === '' || v === NONE || PROJECT_ID.test(v), ''),
    agent: text(row.agent, (v) => v === '' || v === NONE || AGENT_ID.test(v), ''),
    status: text(row.status, (v) => v === '' || (STATUS_ORDER as readonly string[]).includes(v), '') as StatusKey | '',
    search: '',
  });
}

export function serializeRemembered(view: BrowseView): string {
  return JSON.stringify({ layout: view.layout, group: view.group, project: view.project, agent: view.agent, status: view.status });
}

export function isFiltered(view: BrowseView): boolean {
  return view.project !== '' || view.agent !== '' || view.status !== '' || view.search.trim() !== '';
}

/** The view with every filter and the search cleared; the layout and grouping stay. */
export function withoutFilters(view: BrowseView): BrowseView {
  return Object.freeze({ ...view, project: '', agent: '', status: '', search: '' });
}

/** One card with what the filters read, worked out once. */
export interface BrowseRow {
  readonly card: CardRow;
  /** The Needs you lane or a column, or `null` for a card without a ticket that no column fits. */
  readonly status: StatusKey | null;
  readonly project: string | null;
  readonly projectLabel: string | null;
  readonly agent: string | null;
  readonly agentName: string | null;
  /** Lower case, for the search. */
  readonly text: string;
}

function statusOf(card: CardRow): StatusKey | null {
  if (card.ticket === null) return plainColumnOf(card);
  return card.status === 'blocked' ? 'needs' : columnOf(card);
}

export interface Places {
  /** Every project the page knows, to name the ones a ticket was read to be in. */
  readonly projects: readonly { readonly id: string; readonly label: string }[];
  /** The project of each ticket no agent held, once read: ticket id to project id. */
  readonly placed: ReadonlyMap<string, string>;
}

export function rowsOf(floor: FloorState, places: Places): readonly BrowseRow[] {
  const labels = new Map(places.projects.map((project) => [project.id, project.label]));
  return floor.cards.map((card) => {
    const workspace = decodeWorkspace(agentRow(floor, card.claimed_by)?.workspace);
    const placed = places.placed.get(card.id) ?? null;
    const project = workspace?.id ?? placed;
    const label = workspace?.label ?? (placed === null ? null : labels.get(placed) ?? placed);
    const name = card.claimed_by === null ? null : agentName(floor, card.claimed_by);
    return Object.freeze({
      card, status: statusOf(card), project, projectLabel: label, agent: card.claimed_by, agentName: name,
      text: [card.title, name ?? '', label ?? ''].join('\n').toLowerCase(),
    });
  });
}

export interface Choice {
  readonly value: string;
  readonly label: string;
}

export interface Choices {
  readonly projects: readonly Choice[];
  readonly agents: readonly Choice[];
}

const byLabel = (a: Choice, b: Choice) => a.label.localeCompare(b.label) || a.value.localeCompare(b.value);

/** What the project and agent filters offer: every known project and agent, and "none" when some ticket has none. */
export function choicesOf(
  rows: readonly BrowseRow[],
  projects: readonly { readonly id: string; readonly label: string }[],
  agents: readonly { readonly id: string; readonly name: string }[],
): Choices {
  const known = new Map(projects.map((project) => [project.id, project.label]));
  for (const row of rows) if (row.project !== null && !known.has(row.project)) known.set(row.project, row.projectLabel ?? row.project);
  const projectChoices = [...known].map(([value, label]) => ({ value, label })).sort(byLabel);
  const agentChoices = agents.map((agent) => ({ value: agent.id, label: agent.name })).sort(byLabel);
  return Object.freeze({
    projects: Object.freeze(rows.some((row) => row.project === null)
      ? [...projectChoices, { value: NONE, label: 'No project' }] : projectChoices),
    agents: Object.freeze(rows.some((row) => row.agent === null)
      ? [...agentChoices, { value: NONE, label: 'Unassigned' }] : agentChoices),
  });
}

/** The view without a project or agent that no longer exists, so a remembered filter cannot hide everything. */
export function liveView(view: BrowseView, choices: Choices): BrowseView {
  const project = view.project === '' || choices.projects.some((choice) => choice.value === view.project) ? view.project : '';
  const agent = view.agent === '' || choices.agents.some((choice) => choice.value === view.agent) ? view.agent : '';
  return project === view.project && agent === view.agent ? view : Object.freeze({ ...view, project, agent });
}

function matches(row: BrowseRow, view: BrowseView, terms: readonly string[]): boolean {
  if (view.project !== '' && (view.project === NONE ? row.project !== null : row.project !== view.project)) return false;
  if (view.agent !== '' && (view.agent === NONE ? row.agent !== null : row.agent !== view.agent)) return false;
  if (view.status !== '' && row.status !== view.status) return false;
  return terms.every((term) => row.text.includes(term));
}

/** The rows the project, agent and status filters and the search keep. Every word typed must appear. */
export function filterRows(rows: readonly BrowseRow[], view: BrowseView): readonly BrowseRow[] {
  if (!isFiltered(view)) return rows;
  const terms = view.search.toLowerCase().split(/\s+/).filter((term) => term !== '');
  return rows.filter((row) => matches(row, view, terms));
}

export interface Arranged {
  /** Tickets in Needs you, kept apart so the lane can stay first. */
  readonly needs: readonly BrowseRow[];
  /** Everything else in queue order: Ready for you, Working, To do, Done (newest first), then cards no column fits. */
  readonly rest: readonly BrowseRow[];
  readonly doneHidden: number;
}

export function arrange(rows: readonly BrowseRow[], doneShown: number = DONE_SHOWN): Arranged {
  const byId = new Map(rows.map((row) => [row.card.id, row]));
  const cards = rows.map((row) => row.card);
  const { columns, needsYou, doneHidden } = boardColumns(cards, doneShown);
  const rowsFor = (list: readonly CardRow[]) => list.map((card) => byId.get(card.id)!);
  return Object.freeze({
    needs: Object.freeze(rowsFor(needsYou)),
    rest: Object.freeze([...rowsFor(columns.ready), ...rowsFor(columns.working), ...rowsFor(columns.todo),
      ...rowsFor(columns.done), ...rowsFor(otherCards(cards))]),
    doneHidden,
  });
}

export interface RowGroup {
  readonly key: string;
  /** `null` when the list is not grouped. */
  readonly label: string | null;
  readonly rows: readonly BrowseRow[];
}

const OTHER = 'other';

/** `rows` in groups, each keeping the order it came in. Groups sort by name, with "none" last; status groups follow the queue. */
export function groupRows(rows: readonly BrowseRow[], by: GroupBy): readonly RowGroup[] {
  if (by === 'none') return rows.length === 0 ? [] : [{ key: 'all', label: null, rows }];
  const groups = new Map<string, { label: string; rows: BrowseRow[] }>();
  for (const row of rows) {
    const [key, label] = by === 'project' ? [row.project ?? NONE, row.projectLabel ?? 'No project']
      : by === 'agent' ? [row.agent ?? NONE, row.agentName ?? 'Unassigned']
        : [row.status ?? OTHER, row.status === null ? 'Other cards' : STATUS_NAMES[row.status]];
    const group = groups.get(key) ?? { label, rows: [] };
    group.rows.push(row);
    groups.set(key, group);
  }
  const rank = (key: string) => { const at = STATUS_ORDER.indexOf(key as StatusKey); return at === -1 ? STATUS_ORDER.length : at; };
  return [...groups].sort(([aKey, a], [bKey, b]) => {
    if (by === 'status') return rank(aKey) - rank(bKey);
    if (aKey === NONE || bKey === NONE) return aKey === bKey ? 0 : aKey === NONE ? 1 : -1;
    return a.label.localeCompare(b.label) || aKey.localeCompare(bKey);
  }).map(([key, group]) => ({ key, label: group.label, rows: group.rows }));
}

export const GROUP_NAMES: Readonly<Record<GroupBy, string>> = Object.freeze({
  none: 'No grouping', project: 'Project', agent: 'Agent', status: 'Status',
});

export const STATUS_CHOICES: readonly Choice[] = Object.freeze(STATUS_ORDER.map((key) => ({ value: key, label: STATUS_NAMES[key] })));

const CROCKFORD = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';

/** When a card was made, in milliseconds: the daemon mints its id with the time in the first ten characters. */
export function madeAt(id: string): number {
  let time = 0;
  for (const char of id.slice(0, 10)) time = time * 32 + Math.max(0, CROCKFORD.indexOf(char));
  return time;
}

/** How old something is in one short word: now, 5m, 3h, 2d, 6w, 4mo or 1y. */
export function ageWords(since: number, now: number): string {
  const seconds = Math.max(0, Math.floor((now - since) / 1000));
  if (seconds < 60) return 'now';
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d`;
  if (days < 30) return `${Math.floor(days / 7)}w`;
  if (days < 365) return `${Math.floor(days / 30)}mo`;
  return `${Math.floor(days / 365)}y`;
}
