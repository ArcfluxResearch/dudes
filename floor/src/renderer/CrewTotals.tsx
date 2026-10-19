// SPDX-License-Identifier: MIT OR Apache-2.0

import type { AgentRow } from './scene/cast.ts';
import type { CardRow } from './board.ts';
import { crewStatuses } from './crewTotals.ts';

/** The top bar's counts: the crew, then each status some agent has, by the records' own status words. */
export function CrewStats({ agents, cards, statuses = true }: {
  readonly agents: readonly AgentRow[]; readonly cards?: readonly CardRow[];
  /** False while the strip under the office already says who needs you. */
  readonly statuses?: boolean;
}) {
  return (
    <ul className="crew-stats" aria-label="Crew by status">
      <li className="crew-stats-total">{`${agents.length} crew`}</li>
      {(statuses ? crewStatuses(agents, cards) : []).map(({ status, count }) => <li key={status} data-status={status}>
        {`${count} ${status}`}
      </li>)}
    </ul>
  );
}
