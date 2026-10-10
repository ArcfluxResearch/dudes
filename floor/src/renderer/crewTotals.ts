// SPDX-License-Identifier: MIT OR Apache-2.0

import type { AgentRow } from './scene/cast.ts';
import { fieldsOf } from './scene/label.ts';

const STATUS_ORDER = ['running', 'waiting', 'held', 'done', 'idle'];

/** Each status some agent has, with how many, in the order the top bar lists them. */
export function crewStatuses(agents: readonly AgentRow[]): readonly { readonly status: string; readonly count: number }[] {
  const counts = new Map<string, number>();
  for (const row of agents) {
    const status = fieldsOf(row).status || 'unknown';
    counts.set(status, (counts.get(status) ?? 0) + 1);
  }
  const rank = (status: string): number => {
    const at = STATUS_ORDER.indexOf(status);
    return at === -1 ? STATUS_ORDER.length : at;
  };
  return [...counts].map(([status, count]) => ({ status, count }))
    .sort((a, b) => rank(a.status) - rank(b.status) || (a.status < b.status ? -1 : 1));
}
