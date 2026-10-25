// SPDX-License-Identifier: MIT OR Apache-2.0

import type { AgentRow } from './scene/cast.ts';
import type { CardRow } from './board.ts';
import { NEED_LABEL, NEED_STATES, needsOf } from './needs.ts';
import type { NeedState } from './needs.ts';

export function crewStatuses(agents: readonly AgentRow[], cards: readonly CardRow[] = []): readonly {
  readonly status: NeedState; readonly label: string; readonly count: number;
}[] {
  const rows = needsOf(agents, cards);
  return NEED_STATES.map((status) => ({ status, label: NEED_LABEL[status].toLowerCase(),
    count: rows.filter((row) => row.state === status).length })).filter(({ count }) => count > 0);
}
