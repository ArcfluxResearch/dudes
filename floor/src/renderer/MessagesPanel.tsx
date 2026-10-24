// SPDX-License-Identifier: MIT OR Apache-2.0

import { RelativeTime } from './RelativeTime.tsx';
import type { RetiredAgentRow } from './archive.ts';
import { messageGroups } from './messages.ts';
import type { MessageRow, ThreadRow } from './messages.ts';
import type { AgentRow } from './scene/cast.ts';

export function MessagesPanel({ messages, threads, agent, agents, retiredAgents }: {
  readonly messages: readonly MessageRow[];
  readonly threads: readonly ThreadRow[];
  readonly agent: string;
  readonly agents: readonly AgentRow[];
  readonly retiredAgents: readonly RetiredAgentRow[];
}) {
  const names = new Map<string, string>(retiredAgents.map(row => [row.id, row.name]));
  for (const row of agents) {
    if (typeof row.name === 'string' && row.name !== '') names.set(row.id, row.name);
  }
  const name = (id: string) => names.get(id) || id;
  return <div className="messages-panel">
    <p>Recent agent messages. Older messages may be outside this window.</p>
    {messageGroups(messages, threads, agent).map(pair => <section key={pair.key}
      data-message-pair={pair.thread === undefined ? pair.participants.join(':') : undefined}
      data-message-thread={pair.thread}>
      <h3>{pair.participants.map(name).join(' · ')}</h3>
      <ol aria-label={`Messages between ${pair.participants.map(name).join(', ')}`}>
        {pair.rows.map(row => <li key={row.id} data-message-id={row.id}>
          <p><strong>{name(row.from)}</strong> to <strong>{name(row.to)}</strong></p>
          <p><RelativeTime at={row.at} /> · <span>{row.state}</span></p>
          <details><summary>Details</summary><dl><div><dt>Message id</dt><dd>{row.id}</dd></div>
            <div><dt>Creation order</dt><dd>{row.lsn}</dd></div></dl></details>
        </li>)}
      </ol>
    </section>)}
  </div>;
}
