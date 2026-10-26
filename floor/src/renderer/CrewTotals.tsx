// SPDX-License-Identifier: MIT OR Apache-2.0

import type { AgentRow } from './scene/cast.ts';
import type { CardRow } from './board.ts';
import { crewStatuses } from './crewTotals.ts';
import type { NeedState } from './needs.ts';
import { cn } from '../ui/index.ts';

const DOT: Readonly<Record<NeedState, string>> = {
  'needs-you': 'before:bg-warn', ready: 'before:bg-accent', working: 'before:bg-info', waiting: 'before:bg-muted', done: 'before:bg-ok', idle: 'before:bg-subtle',
};

/** The top bar's counts: the crew, then each status some agent has, by the shared state words. */
export function CrewStats({ agents, cards, statuses = true }: {
  readonly agents: readonly AgentRow[]; readonly cards?: readonly CardRow[];
  /** False while the roster beside the office already says who needs you. */
  readonly statuses?: boolean;
}) {
  return (
    <ul className="crew-stats m-0 flex list-none flex-wrap items-center gap-2 p-0 font-sans text-small font-medium tabular-nums" aria-label="Crew by status">
      <li className="crew-stats-total text-muted">{`${agents.length} crew`}</li>
      {(statuses ? crewStatuses(agents, cards) : []).map(({ status, label, count }) => <li key={status} data-status={status}
        className={cn('inline-flex h-6 items-center gap-1.5 rounded-pill border border-line bg-surface-2 px-2 text-ink before:size-1.5 before:rounded-pill before:content-[""]', DOT[status])}>
        {`${count} ${label}`}
      </li>)}
    </ul>
  );
}
