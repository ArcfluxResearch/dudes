// SPDX-License-Identifier: MIT OR Apache-2.0

import { useMemo } from 'react';

import { accentOf, accessoryOf, lookOf } from './appearance.ts';
import type { CardRow } from './board.ts';
import { NEED_LABEL, needsOf } from './needs.ts';
import type { NeedRow } from './needs.ts';
import { Portrait } from './Portrait.tsx';
import type { AgentRow } from './scene/cast.ts';
import { useLiveActivityExpiry } from './useLiveActivityExpiry.ts';
import type { CSSProperties } from 'react';

/**
 * One card per agent along the bottom of the office, the ones that need the operator first. Each card says
 * who, in plain words what state, what the agent is doing, and offers the one click that acts on it.
 */
export function NeedsStrip({ agents, cards, selectedId, selectable, stale, onSelectAgent, onOpenQuestion, onOpenTicket }: {
  readonly agents: readonly AgentRow[];
  readonly cards: readonly CardRow[];
  readonly selectedId: string | null;
  readonly selectable: boolean;
  readonly stale: boolean;
  readonly onSelectAgent: (id: string) => void;
  readonly onOpenQuestion: (id: string) => void;
  readonly onOpenTicket: (id: string) => void;
}) {
  const live = selectable && !stale;
  useLiveActivityExpiry(agents, live);
  const rows = needsOf(agents, cards, { live, now: performance.now(), wallNow: Date.now() });
  const byId = useMemo(() => new Map(agents.map((row) => [row.id, row] as const)), [agents]);
  if (rows.length === 0) return null;
  const ready = rows.filter((row) => row.state === 'ready').length;
  const needing = rows.filter((row) => row.state === 'needs-you').length;
  const waiting = ready + needing;
  const heading = waiting === 0 ? 'Nobody needs you right now'
    : [ready > 0 ? `${ready} ready for you` : null, needing > 0 ? `${needing} need${needing === 1 ? 's' : ''} you` : null]
      .filter((part) => part !== null).join(' · ');

  function act(row: NeedRow): void {
    if (row.action.kind === 'question') onOpenQuestion(row.id);
    else if (row.action.kind === 'ticket') onOpenTicket(row.action.card);
    else onSelectAgent(row.id);
  }

  return (
    <section className="needs-strip" aria-label="Who needs you" data-waiting={waiting}>
      <h2 className="needs-strip-title">{heading}</h2>
      <ul className="needs-cards">
        {rows.map((row) => {
          const agent = byId.get(row.id);
          return (
            <li key={row.id} className="needs-card" data-state={row.state} data-agent-id={row.id}
              data-selected={selectedId === row.id ? 'true' : 'false'}
              style={{ '--agent': agent === undefined ? 'currentColor' : accentOf(agent) } as CSSProperties}>
              <button type="button" className="needs-main" disabled={!selectable}
                aria-label={`${row.name}, ${NEED_LABEL[row.state]}. ${row.doing}`}
                onClick={() => act(row)}>
                {agent === undefined ? null
                  : <Portrait className="needs-face" look={lookOf(agent)} accessory={accessoryOf(agent.role)} name={row.name} />}
                <span className="needs-who">
                  <strong className="needs-name">{row.name}</strong>
                  <span className="needs-chip" data-state={row.state}>{NEED_LABEL[row.state]}</span>
                </span>
                <span className="needs-doing" title={row.doing}>{row.doing}</span>
              </button>
              <div className="needs-acts">
                <button type="button" className="needs-act" data-primary={row.action.kind !== 'agent'}
                  disabled={!selectable} onClick={() => act(row)}>{row.action.label}</button>
                {row.action.kind === 'agent' ? null : <button type="button" className="needs-act needs-chat"
                  disabled={!selectable} aria-label={`Chat with ${row.name}`}
                  onClick={() => onSelectAgent(row.id)}>Chat</button>}
              </div>
            </li>
          );
        })}
      </ul>
    </section>
  );
}
