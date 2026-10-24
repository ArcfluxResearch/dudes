// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ReactNode } from 'react';

import { automaticHoldOf, automaticHoldText } from './automaticHold.ts';
import type { CardRow } from './board.ts';
import { baselineOf, completionCheckOf } from './goal/completionCheck.ts';
import type { Baseline } from './goal/completionCheck.ts';
import { goalIdOf } from './goal/goal.ts';
import { GoalResult } from './goal/GoalResult.tsx';
import { goalResultOf } from './goal/result.ts';
import { HIRE_ENGINES } from './onboarding/catalog.ts';
import type { AgentRow } from './scene/cast.ts';
import type { AgentFields } from './scene/label.ts';
import { reasonSentence, stateName } from './ticket/words.ts';
import { workspaceLabel } from './workspace.ts';
import { decodeWorktree, sharesCheckout, worktreeText } from './worktree.ts';

const BASELINE_LINES: Readonly<Record<Baseline | 'absent', string>> = Object.freeze({
  failed: 'It failed before the agent’s first turn, so a pass shows a change.',
  may_already_pass: 'The operator marked this check as one that may already pass. Done shows only that it passes '
    + 'after the agent’s work.',
  absent: 'It did not run before the agent’s first turn.',
});

function value(value: unknown): string {
  return typeof value === 'string' && value !== '' ? value : '—';
}

function currentTicketOf(row: AgentRow, cards: readonly CardRow[]): CardRow | null {
  const goal = goalIdOf(row);
  return goal === null ? null : cards.find((card) => card.claimed_by === row.id && card.ticket?.goal === goal) ?? null;
}

export function AgentOverview({ row, cards, fields, control, recovery, goalForm, taskForm, liveOutput, onOpenTicket }: {
  readonly row: AgentRow;
  readonly cards: readonly CardRow[];
  readonly fields: AgentFields;
  readonly control: ReactNode;
  readonly recovery: ReactNode;
  readonly goalForm: ReactNode;
  readonly taskForm: ReactNode;
  readonly liveOutput: ReactNode;
  readonly onOpenTicket?: (id: string) => void;
}) {
  const ticket = currentTicketOf(row, cards);
  const goal = typeof row.goal === 'object' && row.goal !== null ? row.goal as Record<string, unknown> : null;
  const automaticHold = automaticHoldOf(row);
  const held = ticket?.status === 'blocked';
  const heldTickets = cards.filter((card) => card.claimed_by === row.id && card.ticket !== null && card.status === 'blocked');
  const needsYou = automaticHold !== null || heldTickets.length > 0;
  const text = ticket?.title ?? fields.goal;
  const worktree = decodeWorktree(row.worktree);
  const completionCheck = completionCheckOf(goal ?? {});
  const baseline = baselineOf(goal ?? {});
  const paused = row.paused === true;
  const now = row.retired === true ? 'Retired'
    : row.halted === true ? (row.halted_by_floor === true
      ? (row.status === 'running' ? 'Stopping this turn' : 'Stopped. Press Resume to let it work again') : 'Halted')
      : paused ? (row.status === 'running' ? 'Pausing after this turn' : 'Paused')
        : fields.question !== '' ? 'Needs you: answer the waiting question'
          : automaticHold !== null ? 'Needs you: this agent is held'
            : held ? 'Needs you: current work is held'
              : heldTickets.length > 0 ? 'Needs you: held work needs review'
                : ticket?.status === 'review' ? 'Finished work is ready for you'
                  : fields.status === 'running' ? `Working${text === '' ? '' : ` on ${text}`}`
                    : fields.status === 'waiting' ? 'Waiting'
                      : fields.status === 'attached' ? 'Attached to the engine'
                        : fields.status === 'idle' || fields.status === 'done'
                          ? text !== '' && fields.goalStatus !== 'achieved' && fields.goalStatus !== 'abandoned'
                            ? 'Waiting for the next step on current work' : 'Idle, ready for a task'
                          : fields.status === '' ? 'Status unavailable' : `Status: ${fields.status}`;
  return <div className="agent-overview">
    <section className="agent-overview-now" aria-label="Now">
      <h3>Now</h3>
      <p className="agent-overview-now-line">{now}</p>
      {control}
      {control !== null ? null : fields.question !== '' ? <button type="button" onClick={(event) => {
        const question = event.currentTarget.closest('.agent-detail')?.querySelector<HTMLElement>('.agent-detail-question');
        question?.focus();
        question?.scrollIntoView({ block: 'nearest' });
      }}>Answer</button> : ticket !== null && (held || ticket.status === 'review') && onOpenTicket !== undefined
        ? <button type="button" onClick={() => onOpenTicket(ticket.id)}>{held ? 'Review held work' : 'Review work'}</button>
        : taskForm === null ? null : <details className="agent-overview-task">
            <summary>Give a task</summary>{taskForm}
          </details>}
    </section>
    {!needsYou ? null : <section className="agent-overview-needs" aria-label="Needs you">
      <h3>Needs you</h3>
      {automaticHold === null ? null : <p>{automaticHoldText(automaticHold, row.provider)}</p>}
      {heldTickets.map((card) => <div className="agent-overview-held" key={card.id}>
        <p><strong>{card.title}</strong></p>
        <p>{reasonSentence(card.ticket?.reason ?? null)}</p>
        {onOpenTicket === undefined || card.id === ticket?.id ? null : <button type="button" onClick={() => onOpenTicket(card.id)}>Review ticket</button>}
      </div>)}
      {recovery}
    </section>}
    <section className="agent-overview-work" aria-label="Current work">
      <div className="agent-overview-section-head"><h3>Current work</h3>
        {ticket !== null && onOpenTicket !== undefined
          ? <button type="button" onClick={() => onOpenTicket(ticket.id)}>Change</button> : null}
      </div>
      <p className="agent-overview-work-text">{text === '' ? 'No task assigned' : text}</p>
      {goal === null ? null : <>
        <dl className="agent-overview-work-status"><div><dt>Status</dt><dd>{ticket !== null ? stateName(ticket.status) : value(goal.status)}</dd></div></dl>
        <GoalResult result={goalResultOf(row.goal)} />
        {goal.status !== 'achieved' || baseline !== 'may_already_pass' ? null
          : <p className="agent-overview-check-caveat">{BASELINE_LINES.may_already_pass}</p>}
        <details className="agent-overview-check"><summary>Completion check{completionCheck === null ? '' : ` · ${completionCheck.label}`}</summary>
          <dl><div><dt>Completion check</dt><dd>{completionCheck === null ? 'No name was recorded for this check.' : <>
            <span>{completionCheck.label}</span>
            <span className="completion-check-identity">{completionCheck.id}</span>
            <span className="completion-check-identity">Revision {completionCheck.revision}</span>
            <p>This is the configured check summary. The command is not shown here.</p>
          </>}<p data-check-baseline={baseline ?? 'absent'}>{BASELINE_LINES[baseline ?? 'absent']}</p></dd></div></dl>
        </details>
      </>}
      {goalForm === null ? null : <details className="agent-overview-change"><summary>{ticket === null ? 'Change' : 'Change goal'}</summary>{goalForm}</details>}
    </section>
    {liveOutput}
    {!sharesCheckout(row) ? null : <p className="agent-overview-shared" role="status"><strong>Shared directory</strong>
      {' '}Another live agent works in this agent's directory, so their edits and checkouts can collide.</p>}
    <details className="agent-overview-details"><summary>Details</summary>
      <dl>
        <div><dt>Engine</dt><dd>{typeof row.provider === 'string'
          ? (HIRE_ENGINES as Readonly<Record<string, string>>)[row.provider] ?? value(row.provider) : '—'}</dd></div>
        <div><dt>Mode</dt><dd>{value(row.mode)}</dd></div>
        <div><dt>Project</dt><dd>{workspaceLabel(row)}</dd></div>
        {worktree === null ? null : <div><dt>Works in</dt><dd>{worktreeText(worktree)}</dd></div>}
        <div><dt>Role</dt><dd>{value(row.role)}</dd></div>
        <div><dt>Breaker</dt><dd>{value(row.breaker)}</dd></div>
        <div><dt>Turns</dt><dd>{typeof row.turns === 'number' ? row.turns : 0}</dd></div>
        <div><dt>Cost today</dt><dd>{fields.cost}</dd></div>
        <div><dt>AgentId</dt><dd className="data">{row.id}</dd></div>
      </dl>
      {needsYou ? null : recovery}
    </details>
  </div>;
}
