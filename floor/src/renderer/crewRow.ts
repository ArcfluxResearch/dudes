// SPDX-License-Identifier: MIT OR Apache-2.0

import { automaticHoldOf, automaticHoldText } from './automaticHold.ts';
import type { AgentRow } from './scene/cast.ts';
import { accentOf } from './appearance.ts';
import { ASKING, fieldsOf } from './scene/label.ts';
import { activeCard } from './board.ts';
import type { CardRow } from './board.ts';
import { workspaceLabel } from './workspace.ts';
import { ticketWord } from './ticket/model.ts';
import { checkoutOf, SHARED_DIRECTORY_TEXT } from './worktree.ts';
import type { CheckoutOwnership } from './worktree.ts';
import type { LiveActivityContext } from './liveActivity.ts';

/**
 * `line` is what the row shows, and CSS cuts it to the row's width. `text`
 * is the whole question or goal for hover. `spoken` is the description's words.
 */
export interface Activity {
  readonly kind: 'question' | 'goal' | 'none' | 'hold';
  readonly line: string;
  readonly text: string | undefined;
  readonly spoken: string;
}

/** A standing control flag. A halt outranks a pause, because the Floor can resume a pause but cannot lift a halt. */
export type Hold = 'paused' | 'halted' | 'held';

export interface CrewRow {
  readonly name: string;
  readonly colour: string;
  readonly role: string;
  readonly engine: string;
  readonly workspace: string;
  /** The record's own status token, as the office panel draws it. */
  readonly status: string;
  readonly activity: Activity;
  readonly liveActivity: string | null;
  readonly activeCard: CardRow | null;
  readonly hold: Hold | null;
  readonly checkout: CheckoutOwnership;
  /** Everything the row says but its name, and what it leaves out, in full, for `aria-describedby`. */
  readonly description: string;
}

function shown(value: unknown): string {
  return typeof value === 'string' && value !== '' ? value : '—';
}

function activityOf(question: string, goal: string): Activity {
  if (question !== '') {
    return { kind: 'question', line: `${ASKING}${question}`, text: question, spoken: `Waiting question: ${question}` };
  }
  if (goal !== '') return { kind: 'goal', line: goal, text: goal, spoken: `Goal: ${goal}` };
  return { kind: 'none', line: 'No goal', text: undefined, spoken: 'No goal' };
}

/** The Crew row for one projection row. It reads nothing from the office, so it draws with no canvas at all. */
export function crewRowOf(row: AgentRow, cards: readonly CardRow[] = [], context?: LiveActivityContext): CrewRow {
  const fields = fieldsOf(row, context);
  const automaticHold = automaticHoldOf(row);
  const reason = automaticHold === null ? null : automaticHoldText(automaticHold);
  const activity: Activity = reason === null ? activityOf(fields.question, fields.goal)
    : { kind: 'hold', line: reason, text: reason, spoken: reason };
  const hold = row.halted === true ? 'halted' : row.paused === true ? 'paused'
    : automaticHold === null ? null : 'held';
  const task = activeCard(cards, row.id);
  const engine = shown(row.provider);
  const workspace = workspaceLabel(row);
  const liveActivity = fields.liveActivity;
  const checkout = checkoutOf(row);
  const facts = [fields.role, engine === '—' ? '' : engine, fields.status, `${fields.usd} spent today`, hold ?? '']
    .filter((fact) => fact !== '');
  return {
    name: shown(row.name),
    colour: accentOf(row),
    role: shown(fields.role),
    engine,
    workspace,
    status: shown(ticketWord(cards, row.id, fields.status) ?? fields.status),
    activity,
    liveActivity,
    activeCard: task,
    hold,
    checkout,
    description: `${facts.join(', ')}. ${activity.spoken}${liveActivity === null ? '' : `. ${liveActivity}`}${task === null ? '' : `. Active card: ${task.title}`}. Workspace: ${workspace}${checkout.worktree === null ? '' : `. ${checkout.worktree}`}${checkout.shared ? `. ${SHARED_DIRECTORY_TEXT}` : ''}`,
  };
}
