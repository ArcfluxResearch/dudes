// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ReactNode } from 'react';

import { automaticHoldOf, automaticHoldText } from './automaticHold.ts';
import type { CardRow } from './board.ts';
import { baselineOf, completionCheckOf } from './goal/completionCheck.ts';
import type { Baseline } from './goal/completionCheck.ts';
import { GoalResult } from './goal/GoalResult.tsx';
import { goalResultOf } from './goal/result.ts';
import { HIRE_ENGINES } from './onboarding/catalog.ts';
import type { AgentRow } from './scene/cast.ts';
import type { AgentFields } from './scene/label.ts';
import { currentTicketOf, currentWorkOf } from './agentState.ts';
import { finishedOf, finishedWords } from './needs.ts';
import { reasonSentence, stateName } from './ticket/words.ts';
import { lastTurnOf } from './lastTurn.ts';
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

export function AgentOverview({ row, cards, fields, recovery, goalForm, liveOutput, onOpenTicket }: {
  readonly row: AgentRow;
  readonly cards: readonly CardRow[];
  readonly fields: AgentFields;
  readonly recovery: ReactNode;
  readonly goalForm: ReactNode;
  readonly liveOutput: ReactNode;
  readonly onOpenTicket?: (id: string) => void;
}) {
  const ticket = currentTicketOf(row, cards);
  const finished = finishedOf(row, cards);
  const lastTurn = lastTurnOf(row);
  const goal = typeof row.goal === 'object' && row.goal !== null ? row.goal as Record<string, unknown> : null;
  const automaticHold = automaticHoldOf(row);
  const heldTickets = cards.filter((card) => card.claimed_by === row.id && card.ticket !== null && card.status === 'blocked');
  const needsYou = automaticHold !== null || heldTickets.length > 0;
  const text = currentWorkOf(row, cards, fields);
  const worktree = decodeWorktree(row.worktree);
  const completionCheck = completionCheckOf(goal ?? {});
  const baseline = baselineOf(goal ?? {});
  const record = goal === null ? null : <>
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
  </>;
  return <div className="agent-overview">
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
        {ticket !== null && finished === null && onOpenTicket !== undefined
          ? <button type="button" onClick={() => onOpenTicket(ticket.id)}>Change</button> : null}
      </div>
      <p className="agent-overview-work-text">{finished !== null ? 'None' : text === '' ? 'No task assigned' : text}</p>
      {finished !== null ? null : record}
      {goalForm === null ? null : <details className="agent-overview-change"><summary>{ticket === null || finished !== null ? 'Change' : 'Change goal'}</summary>{goalForm}</details>}
    </section>
    {finished === null ? null : <section className="agent-overview-finished" aria-label="Last finished">
      <div className="agent-overview-section-head"><h3>Last finished</h3>
        {ticket !== null && onOpenTicket !== undefined
          ? <button type="button" onClick={() => onOpenTicket(ticket.id)}>Open ticket</button> : null}
      </div>
      <p className="agent-overview-work-text">{finished.text}</p>
      <p className="agent-overview-finished-words">{finishedWords(finished)}</p>
      {record}
    </section>}
    {lastTurn === null ? null : <section className="agent-overview-usage" aria-label="Last turn">
      <h3>Last turn</h3>
      <dl>
        <div><dt>Tokens</dt><dd>{lastTurn.tokens}{lastTurn.breakdown === null ? null : <small>{lastTurn.breakdown}</small>}</dd></div>
        <div><dt>Cost</dt><dd>{lastTurn.cost.replace('Cost: ', '')}</dd></div>
        <div><dt>Context</dt><dd>{lastTurn.gauge === null ? lastTurn.contextNote.replace('Context: ', '') : <>
          <span className="usage-gauge" role="meter" aria-label="Context used" aria-valuemin={0} aria-valuemax={100}
            aria-valuenow={lastTurn.gauge.percent} aria-valuetext={lastTurn.gauge.text}>
            <span style={{ width: `${lastTurn.gauge.percent}%` }} data-high={lastTurn.gauge.percent >= 80 ? 'true' : 'false'} />
          </span>
          <small>{lastTurn.gauge.text}</small></>}</dd></div>
      </dl>
    </section>}
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
