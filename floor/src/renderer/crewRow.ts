// SPDX-License-Identifier: MIT OR Apache-2.0

import { automaticHoldOf, automaticHoldText } from './automaticHold.ts';
import type { AgentRow } from './scene/cast.ts';
import { accentOf } from './appearance.ts';
import { ASKING, fieldsOf } from './scene/label.ts';
import { activeCard } from './board.ts';
import type { CardRow } from './board.ts';
import { engineName } from './engines.ts';
import { workspaceLabel } from './workspace.ts';
import type { LiveActivityContext } from './liveActivity.ts';

/**
 * `line` is what the row shows, and CSS cuts it to the row's width. `text`
 * is the whole question or goal for hover.
 */
export interface Activity {
  readonly kind: 'question' | 'goal' | 'none' | 'hold';
  readonly line: string;
  readonly text: string | undefined;
}

/** A standing control flag. A halt outranks a pause, because the Floor can resume a pause but cannot lift a halt the operator made. */
export type Hold = 'paused' | 'halted' | 'held';

export interface CrewRow {
  readonly name: string;
  readonly colour: string;
  readonly role: string;
  readonly engine: string;
  readonly workspace: string;
  readonly activity: Activity;
  readonly liveActivity: string | null;
  readonly activeCard: CardRow | null;
  readonly hold: Hold | null;
}

function shown(value: unknown): string {
  return typeof value === 'string' && value !== '' ? value : '—';
}

function activityOf(question: string, goal: string): Activity {
  if (question !== '') {
    return { kind: 'question', line: `${ASKING}${question}`, text: question };
  }
  if (goal !== '') return { kind: 'goal', line: goal, text: goal };
  return { kind: 'none', line: 'No goal', text: undefined };
}

/** The facts one roster row draws from a projection row. It reads nothing from the office, so it draws with no canvas at all. */
export function crewRowOf(row: AgentRow, cards: readonly CardRow[] = [], context?: LiveActivityContext): CrewRow {
  const fields = fieldsOf(row, context);
  const automaticHold = automaticHoldOf(row);
  const reason = automaticHold === null ? null : automaticHoldText(automaticHold, row.provider);
  const activity: Activity = reason === null ? activityOf(fields.question, fields.goal)
    : { kind: 'hold', line: reason, text: reason };
  const hold = row.halted === true ? 'halted' : row.paused === true ? 'paused'
    : automaticHold === null ? null : 'held';
  const task = activeCard(cards, row.id);
  const engine = typeof row.provider === 'string' && row.provider !== '' ? engineName(row.provider) : '—';
  const workspace = workspaceLabel(row);
  const liveActivity = fields.liveActivity;
  return {
    name: shown(row.name),
    colour: accentOf(row),
    role: shown(fields.role),
    engine,
    workspace,
    activity,
    liveActivity,
    activeCard: task,
    hold,
  };
}
