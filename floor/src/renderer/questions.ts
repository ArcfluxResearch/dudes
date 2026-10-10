// SPDX-License-Identifier: MIT OR Apache-2.0

import { canonicalLsn } from './answer/answer.ts';
import type { AgentRow } from './scene/cast.ts';
import { fieldsOf } from './scene/label.ts';

export interface OpenQuestion {
  readonly row: AgentRow;
  readonly name: string;
  readonly text: string;
  readonly lsn: bigint | null;
}

export function openQuestions(agents: readonly AgentRow[]): readonly OpenQuestion[] {
  return agents.flatMap((row): OpenQuestion[] => {
    const text = canonicalLsn(row.question_lsn) && typeof row.question === 'string'
      ? row.question : fieldsOf(row).question;
    if (text === '') return [];
    const name = typeof row.name === 'string' && row.name !== '' ? row.name : row.id;
    return [{ row, name, text, lsn: canonicalLsn(row.question_lsn) ? BigInt(row.question_lsn) : null }];
  }).sort(newestFirstUnplacedLast);
}

function newestFirstUnplacedLast(a: OpenQuestion, b: OpenQuestion): number {
  if (a.lsn === b.lsn) return 0;
  if (a.lsn === null) return 1;
  if (b.lsn === null) return -1;
  return a.lsn > b.lsn ? -1 : 1;
}

/** One thing that waits on the operator. Question is the only kind today. */
export type WaitingItem = {
  readonly kind: 'question';
  readonly key: string;
  readonly agentId: string;
  readonly name: string;
  readonly text: string;
  readonly waitedMs: number;
};

/*
 * The projection carries no time for a question, so a wait is counted from the
 * moment this page first saw it open. A reload starts the count again.
 */
const firstSeen = new Map<string, number>();

function keyOf(question: OpenQuestion): string {
  return `${question.row.id}:${question.lsn ?? question.text}`;
}

export function noteWaiting(agents: readonly AgentRow[], now: number): void {
  const open = openQuestions(agents).map(keyOf);
  for (const key of firstSeen.keys()) if (!open.includes(key)) firstSeen.delete(key);
  for (const key of open) if (!firstSeen.has(key)) firstSeen.set(key, now);
}

export function resetWaiting(): void {
  firstSeen.clear();
}

/** Everything waiting on the operator, newest question first. */
export function waitingOnYou(agents: readonly AgentRow[], now: number): readonly WaitingItem[] {
  noteWaiting(agents, now);
  return openQuestions(agents).map((question): WaitingItem => {
    const key = keyOf(question);
    return {
      kind: 'question', key, agentId: question.row.id, name: question.name, text: question.text,
      waitedMs: Math.max(0, now - (firstSeen.get(key) ?? now)),
    };
  });
}

export function waitedLabel(waitedMs: number): string {
  const minutes = Math.floor(waitedMs / 60_000);
  if (minutes < 1) return 'waiting under a minute';
  if (minutes < 60) return `waiting ${minutes} min`;
  const rest = minutes % 60;
  return `waiting ${Math.floor(minutes / 60)} h${rest === 0 ? '' : ` ${rest} min`}`;
}
