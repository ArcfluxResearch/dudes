// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the Projects page shows for each project, worked out from the snapshot alone: the agents hired
// into it, its tickets by place on the Board, its completion check, whether the operator trusts it and
// the limits that apply to it. A fact whose source the snapshot did not carry is `null`, never a guess.

import type { Hold } from '../crewRow.ts';
import type { FloorState } from '../floor/store.ts';
import { NEED_LABEL, needsOf } from '../needs.ts';
import type { NeedState } from '../needs.ts';
import { formatUsdMicros } from '../onboarding/catalog.ts';
import { rowsOf } from '../ticket/browse.ts';
import type { StatusKey } from '../ticket/browse.ts';
import { decodeWorkspace } from '../workspace.ts';

export interface ProjectAgent {
  readonly id: string;
  readonly name: string;
  readonly state: NeedState;
  /** The word beside the name: a pause, stop or hold outranks the state it interrupts. */
  readonly word: string;
}

/** The order the Board lays its places out in, left to right. */
export const TICKET_PLACES: readonly { readonly key: StatusKey; readonly title: string }[] = Object.freeze([
  { key: 'needs', title: 'Needs you' }, { key: 'todo', title: 'To do' }, { key: 'working', title: 'Working' },
  { key: 'ready', title: 'Ready for you' }, { key: 'done', title: 'Done' },
]);

export interface ProjectRow {
  readonly id: string;
  readonly label: string;
  readonly agents: readonly ProjectAgent[];
  readonly tickets: Readonly<Record<StatusKey, number>>;
  /** The label of the check that decides when work in this project is done, or `null` when the snapshot names none. */
  readonly check: string | null;
  /** Whether the operator trusts the project for own-copy hires, or `null` when the snapshot says nothing about it. */
  readonly trusted: boolean | null;
  /** The limits that are set, worded for reading. Empty means no cap. `null` means the snapshot carries no limits. */
  readonly limits: readonly string[] | null;
}

const HOLD_WORD: Readonly<Record<Hold, string>> = Object.freeze({ paused: 'Paused', halted: 'Stopped', held: 'Held' });

interface Ceilings {
  readonly agentUsdPerDayMicros: string | null;
  readonly agentTokensPerDay: string | null;
  readonly goalUsdMicros: string | null;
  readonly goalTokens: string | null;
}

function limitLines(ceilings: Ceilings): readonly string[] {
  return Object.freeze([
    ceilings.agentUsdPerDayMicros === null ? null : `${formatUsdMicros(ceilings.agentUsdPerDayMicros)} a day for each agent`,
    ceilings.agentTokensPerDay === null ? null : `${ceilings.agentTokensPerDay} tokens a day for each agent`,
    ceilings.goalUsdMicros === null ? null : `${formatUsdMicros(ceilings.goalUsdMicros)} for a new task`,
    ceilings.goalTokens === null ? null : `${ceilings.goalTokens} tokens for a new task`,
  ].filter((line) => line !== null));
}

/** Every project the snapshot knows, with its label, in label order. */
function knownProjects(floor: FloorState): readonly { readonly id: string; readonly label: string }[] {
  const labels = new Map<string, string>();
  for (const row of floor.settings?.workspaces ?? []) labels.set(row.workspace_id, row.label);
  for (const row of floor.onboarding?.workspaces ?? []) if (!labels.has(row.workspaceId)) labels.set(row.workspaceId, row.label);
  for (const row of floor.checks?.workspaces ?? []) {
    if (labels.has(row.workspaceId)) continue;
    const named = floor.agents.map((agent) => decodeWorkspace(agent.workspace)).find((space) => space?.id === row.workspaceId);
    labels.set(row.workspaceId, named?.label ?? row.workspaceId);
  }
  return [...labels].map(([id, label]) => ({ id, label }))
    .sort((a, b) => a.label.localeCompare(b.label) || (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
}

/**
 * One row for each enrolled project. `placed` is the project of each ticket no agent holds yet, as the
 * Board reads it, so the two pages count the same tickets.
 */
export function projectRowsOf(floor: FloorState, placed: ReadonlyMap<string, string>): readonly ProjectRow[] {
  const projects = knownProjects(floor);
  const needs = needsOf(floor.agents, floor.cards, undefined, floor.stepLabels);
  const rows = rowsOf(floor, { projects, placed }).filter((row) => row.card.ticket !== null);
  const workspaceOf = new Map(floor.agents.map((agent) => [agent.id, decodeWorkspace(agent.workspace)?.id ?? null]));
  return Object.freeze(projects.map(({ id, label }) => {
    const agents = needs.filter((need) => workspaceOf.get(need.id) === id).map((need) => Object.freeze({
      id: need.id, name: need.name, state: need.state,
      word: need.crew.hold === null ? NEED_LABEL[need.state] : HOLD_WORD[need.crew.hold],
    }));
    const tickets: Record<StatusKey, number> = { needs: 0, todo: 0, working: 0, ready: 0, done: 0 };
    for (const row of rows) if (row.project === id && row.status !== null) tickets[row.status] += 1;
    const settings = floor.settings?.workspaces.find((row) => row.workspace_id === id);
    const enrolled = floor.onboarding?.workspaces.find((row) => row.workspaceId === id);
    const ceilings: Ceilings | null = settings !== undefined ? {
      agentUsdPerDayMicros: settings.agent_usd_per_day_micros, agentTokensPerDay: settings.agent_tokens_per_day,
      goalUsdMicros: settings.goal_usd_micros, goalTokens: settings.goal_tokens,
    } : enrolled ?? null;
    return Object.freeze({
      id, label, agents: Object.freeze(agents), tickets: Object.freeze(tickets),
      check: floor.checks?.workspaces.find((row) => row.workspaceId === id)?.checks[0]?.label ?? null,
      trusted: enrolled?.trusted ?? null,
      limits: ceilings === null ? null : limitLines(ceilings),
    });
  }));
}
