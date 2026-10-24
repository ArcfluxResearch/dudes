// SPDX-License-Identifier: MIT OR Apache-2.0

import { stateName } from './ticket/words.ts';
import type { CardStatus } from './board.ts';

const ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;
const POSITION = /^[1-9][0-9]{0,19}$/;
const MAX_LSN = 18_446_744_073_709_551_615n;
export const MAX_ACTIVITY_ROWS = 64;
export const MAX_ACTIVITY_BYTES = 32_768;
const UTF8 = new TextEncoder();

export interface ActivityAgent {
  readonly id: string;
  readonly name?: string;
}
export interface ActivityCard {
  readonly id: string;
  readonly title: string;
  readonly status: CardStatus;
}
type AgentEvent = { readonly kind: 'turn_issued' | 'turn_ended' | 'goal_set' | 'goal_done'
  | 'human_question' | 'human_answer' | 'agent_renamed' | 'agent_retired' | 'pause' | 'resume'; readonly agent: ActivityAgent };
type CardEvent = { readonly kind: 'card_created' | 'card_transition'; readonly card: ActivityCard }
  | { readonly kind: 'card_claimed'; readonly card: ActivityCard; readonly agent: ActivityAgent };
export type ActivityRow = { readonly lsn: string; readonly at: number } & (AgentEvent | CardEvent);

function object(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value) ? value as Record<string, unknown> : null;
}
function agentOf(value: unknown): ActivityAgent | null {
  const source = object(value);
  if (source === null || typeof source.id !== 'string' || !ID.test(source.id)
    || (source.name !== undefined && typeof source.name !== 'string')) return null;
  return Object.freeze({ id: source.id, ...(source.name === undefined ? {} : { name: source.name as string }) });
}
function cardOf(value: unknown): ActivityCard | null {
  const source = object(value);
  if (source === null || typeof source.id !== 'string' || !ID.test(source.id)
    || typeof source.title !== 'string' || typeof source.status !== 'string'
    || !['todo', 'claimed', 'doing', 'review', 'blocked', 'done'].includes(source.status)) return null;
  return Object.freeze({ id: source.id, title: source.title, status: source.status as CardStatus });
}
function rowOf(value: unknown): ActivityRow | null {
  const source = object(value);
  if (source === null || typeof source.lsn !== 'string' || !POSITION.test(source.lsn)
    || BigInt(source.lsn) > MAX_LSN || typeof source.at !== 'number' || !Number.isInteger(source.at)) return null;
  const base = { lsn: source.lsn, at: source.at };
  switch (source.kind) {
    case 'turn_issued': case 'turn_ended': case 'goal_set': case 'goal_done':
    case 'human_question': case 'human_answer': case 'agent_renamed': case 'agent_retired': case 'pause': case 'resume': {
      const agent = agentOf(source.agent);
      return agent === null ? null : Object.freeze({ ...base, kind: source.kind, agent });
    }
    case 'card_created': case 'card_transition': {
      const card = cardOf(source.card);
      return card === null ? null : Object.freeze({ ...base, kind: source.kind, card });
    }
    case 'card_claimed': {
      const card = cardOf(source.card);
      const agent = agentOf(source.agent);
      return card === null || agent === null ? null : Object.freeze({ ...base, kind: source.kind, card, agent });
    }
    default: return null;
  }
}

export function decodeActivity(value: unknown): { readonly rows: readonly ActivityRow[]; readonly rejected: number } {
  if (!Array.isArray(value)) return { rows: Object.freeze([]), rejected: value === undefined ? 0 : 1 };
  if (value.length > MAX_ACTIVITY_ROWS || UTF8.encode(JSON.stringify(value)).length > MAX_ACTIVITY_BYTES) {
    return { rows: Object.freeze([]), rejected: value.length || 1 };
  }
  const rows: ActivityRow[] = [];
  let rejected = 0;
  let last = 0n;
  for (const raw of value) {
    const row = rowOf(raw);
    if (row === null || BigInt(row.lsn) <= last) rejected += 1;
    else { rows.push(row); last = BigInt(row.lsn); }
  }
  return { rows: Object.freeze(rows), rejected };
}

export function activityText(row: ActivityRow): string {
  const name = 'agent' in row ? row.agent.name || 'An agent' : '';
  switch (row.kind) {
    case 'turn_issued': return `Dudes asked ${name} to work`;
    case 'turn_ended': return `${name} finished a work attempt`;
    case 'goal_set': return `${name} received a task`;
    case 'goal_done': return `${name} completed a task`;
    case 'human_question': return `${name} asked a question`;
    case 'human_answer': return `${name} received an answer`;
    case 'agent_renamed': return `${name} was renamed`;
    case 'agent_retired': return `${name} retired`;
    case 'pause': return `${name} paused`;
    case 'resume': return `${name} resumed`;
    case 'card_created': return `Created ${row.card.title} · ${stateName(row.card.status)}`;
    case 'card_transition': return `Moved ${row.card.title} · ${stateName(row.card.status)}`;
    case 'card_claimed': return `${name} was assigned ${row.card.title} · ${stateName(row.card.status)}`;
  }
}

export const ACTIVITY_LABELS: Readonly<Record<ActivityRow['kind'], string>> = Object.freeze({
  turn_issued: 'Work started', turn_ended: 'Work ended', goal_set: 'Task given', goal_done: 'Task completed',
  human_question: 'Question asked', human_answer: 'Question answered', agent_renamed: 'Agent renamed',
  agent_retired: 'Agent retired', pause: 'Agent paused', resume: 'Agent resumed',
  card_created: 'Ticket created', card_transition: 'Ticket moved', card_claimed: 'Ticket assigned',
});
