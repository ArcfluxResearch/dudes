// SPDX-License-Identifier: MIT OR Apache-2.0

import type { AgentRow } from './scene/cast.ts';
import { crewStatuses } from './crewTotals.ts';

/** The top bar's counts: the crew, then each status some agent has, by the records' own status words. */
export function CrewStats({ agents }: { readonly agents: readonly AgentRow[] }) {
  return (
    <ul className="crew-stats" aria-label="Crew by status">
      <li className="crew-stats-total">{`${agents.length} crew`}</li>
      {crewStatuses(agents).map(({ status, count }) => <li key={status} data-status={status}>
        {`${count} ${status}`}
      </li>)}
    </ul>
  );
}
