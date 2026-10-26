// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ReactNode } from 'react';

import { Button, Disclosure, cn } from '../ui/index.ts';
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

const HEADING = 'm-0 font-sans text-base font-semibold text-ink';
const BODY = 'm-0 text-body text-ink whitespace-pre-wrap [overflow-wrap:anywhere]';
const FACT_LIST = 'm-0 grid grid-cols-[minmax(0,7rem)_minmax(0,1fr)] gap-x-3 gap-y-1.5 text-base';
const FACT_ROW = 'contents';
const FACT_TERM = 'text-base font-normal normal-case tracking-normal text-muted';
const FACT_DEF = 'm-0 min-w-0 text-ink [overflow-wrap:anywhere]';

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
    <dl className={cn('agent-overview-work-status', FACT_LIST)}><div className={FACT_ROW}><dt className={FACT_TERM}>Status</dt><dd className={FACT_DEF}>{ticket !== null ? stateName(ticket.status) : value(goal.status)}</dd></div></dl>
    <GoalResult result={goalResultOf(row.goal)} />
    {goal.status !== 'achieved' || baseline !== 'may_already_pass' ? null
      : <p className="agent-overview-check-caveat m-0 text-base text-muted">{BASELINE_LINES.may_already_pass}</p>}
    <Disclosure className="agent-overview-check" summary={`Completion check${completionCheck === null ? '' : ` · ${completionCheck.label}`}`}>
      <dl className={FACT_LIST}><div className={FACT_ROW}><dt className={FACT_TERM}>Completion check</dt><dd className={cn(FACT_DEF, 'grid gap-1')}>{completionCheck === null ? 'No name was recorded for this check.' : <>
        <span>{completionCheck.label}</span>
        <span className="completion-check-identity block font-mono text-small text-muted [overflow-wrap:anywhere]">{completionCheck.id}</span>
        <span className="completion-check-identity block font-mono text-small text-muted [overflow-wrap:anywhere]">Revision {completionCheck.revision}</span>
        <p className="m-0 text-muted">This is the configured check summary. The command is not shown here.</p>
      </>}<p className="m-0 text-muted" data-check-baseline={baseline ?? 'absent'}>{BASELINE_LINES[baseline ?? 'absent']}</p></dd></div></dl>
    </Disclosure>
  </>;
  return <div className="agent-overview grid gap-5 font-sans text-base text-ink">
    {!needsYou ? null : <section className="agent-overview-needs grid gap-2 rounded-lg border border-warn/40 bg-warn-soft p-3" aria-label="Needs you">
      <h3 className={HEADING}>Needs you</h3>
      {automaticHold === null ? null : <p className="m-0 text-body">{automaticHoldText(automaticHold, row.provider)}</p>}
      {heldTickets.map((card) => <div className="agent-overview-held grid justify-items-start gap-2" key={card.id}>
        <p className="m-0 text-body"><strong>{card.title}</strong></p>
        <p className="m-0 text-body">{reasonSentence(card.ticket?.reason ?? null)}</p>
        {onOpenTicket === undefined || card.id === ticket?.id ? null : <Button size="sm" onClick={() => onOpenTicket(card.id)}>Review ticket</Button>}
      </div>)}
      {recovery}
    </section>}
    <section className="agent-overview-work grid min-w-0 gap-2" aria-label="Current work">
      <div className="agent-overview-section-head flex items-center justify-between gap-3"><h3 className={HEADING}>Current work</h3>
        {ticket !== null && finished === null && onOpenTicket !== undefined
          ? <Button variant="ghost" size="sm" onClick={() => onOpenTicket(ticket.id)}>Change</Button> : null}
      </div>
      <p className={cn('agent-overview-work-text', BODY)}>{finished !== null ? 'None' : text === '' ? 'No task assigned' : text}</p>
      {finished !== null ? null : record}
      {goalForm === null ? null : <Disclosure className="agent-overview-change" summary={ticket === null || finished !== null ? 'Change' : 'Change goal'}>{goalForm}</Disclosure>}
    </section>
    {finished === null ? null : <section className="agent-overview-finished grid min-w-0 gap-2" aria-label="Last finished">
      <div className="agent-overview-section-head flex items-center justify-between gap-3"><h3 className={HEADING}>Last finished</h3>
        {ticket !== null && onOpenTicket !== undefined
          ? <Button variant="ghost" size="sm" onClick={() => onOpenTicket(ticket.id)}>Open ticket</Button> : null}
      </div>
      <p className={cn('agent-overview-work-text', BODY)}>{finished.text}</p>
      <p className="agent-overview-finished-words m-0 text-base text-muted">{finishedWords(finished)}</p>
      {record}
    </section>}
    {lastTurn === null ? null : <section className="agent-overview-usage grid min-w-0 gap-2" aria-label="Last turn">
      <h3 className={HEADING}>Last turn</h3>
      <dl className={FACT_LIST}>
        <div className={FACT_ROW}><dt className={FACT_TERM}>Tokens</dt><dd className={cn(FACT_DEF, 'grid gap-0.5')}>{lastTurn.tokens}{lastTurn.breakdown === null ? null : <small className="text-small text-muted">{lastTurn.breakdown}</small>}</dd></div>
        <div className={FACT_ROW}><dt className={FACT_TERM}>Cost</dt><dd className={FACT_DEF}>{lastTurn.cost.replace('Cost: ', '')}</dd></div>
        <div className={FACT_ROW}><dt className={FACT_TERM}>Context</dt><dd className={cn(FACT_DEF, 'grid gap-1')}>{lastTurn.gauge === null ? lastTurn.contextNote.replace('Context: ', '') : <>
          <span className="usage-gauge block h-1.5 overflow-hidden rounded-pill bg-surface-2" role="meter" aria-label="Context used" aria-valuemin={0} aria-valuemax={100}
            aria-valuenow={lastTurn.gauge.percent} aria-valuetext={lastTurn.gauge.text}>
            <span className={cn('block h-full rounded-pill', lastTurn.gauge.percent >= 80 ? 'bg-warn' : 'bg-accent')} style={{ width: `${lastTurn.gauge.percent}%` }} data-high={lastTurn.gauge.percent >= 80 ? 'true' : 'false'} />
          </span>
          <small className="text-small text-muted">{lastTurn.gauge.text}</small></>}</dd></div>
      </dl>
    </section>}
    {liveOutput}
    {!sharesCheckout(row) ? null : <p className="agent-overview-shared m-0 rounded-md border border-line border-l-2 border-l-warn bg-surface-2 px-3 py-2 text-base" role="status"><strong>Shared directory</strong>
      {' '}Another live agent works in this agent's directory, so their edits and checkouts can collide.</p>}
    <Disclosure className="agent-overview-details border-t border-line pt-3" summary="Details">
      <dl className={FACT_LIST}>
        <div className={FACT_ROW}><dt className={FACT_TERM}>Engine</dt><dd className={FACT_DEF}>{typeof row.provider === 'string'
          ? (HIRE_ENGINES as Readonly<Record<string, string>>)[row.provider] ?? value(row.provider) : '—'}</dd></div>
        <div className={FACT_ROW}><dt className={FACT_TERM}>Mode</dt><dd className={FACT_DEF}>{value(row.mode)}</dd></div>
        <div className={FACT_ROW}><dt className={FACT_TERM}>Project</dt><dd className={FACT_DEF}>{workspaceLabel(row)}</dd></div>
        {worktree === null ? null : <div className={FACT_ROW}><dt className={FACT_TERM}>Works in</dt><dd className={FACT_DEF}>{worktreeText(worktree)}</dd></div>}
        <div className={FACT_ROW}><dt className={FACT_TERM}>Role</dt><dd className={FACT_DEF}>{value(row.role)}</dd></div>
        <div className={FACT_ROW}><dt className={FACT_TERM}>Breaker</dt><dd className={FACT_DEF}>{value(row.breaker)}</dd></div>
        <div className={FACT_ROW}><dt className={FACT_TERM}>Turns</dt><dd className={FACT_DEF}>{typeof row.turns === 'number' ? row.turns : 0}</dd></div>
        <div className={FACT_ROW}><dt className={FACT_TERM}>Cost today</dt><dd className={FACT_DEF}>{fields.cost}</dd></div>
        <div className={FACT_ROW}><dt className={FACT_TERM}>AgentId</dt><dd className={cn(FACT_DEF, 'data font-mono text-small')}>{row.id}</dd></div>
      </dl>
      {needsYou ? null : recovery}
    </Disclosure>
  </div>;
}
