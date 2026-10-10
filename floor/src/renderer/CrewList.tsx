// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, type CSSProperties, type ReactNode } from 'react';

import { ReorderHandle, useAgentOrder } from './AgentOrder.tsx';
import type { AgentRow } from './scene/cast.ts';
import { crewRowOf } from './crewRow.ts';
import { initialOf } from './Portrait.tsx';
import { useLiveActivityExpiry } from './useLiveActivityExpiry.ts';
import type { CardRow } from './board.ts';
import { decodeWorkspace } from './workspace.ts';
import { checkoutSummary, SHARED_DIRECTORY_TEXT } from './worktree.ts';

export function crewRowId(agent: string): string {
  return `crew-row-${agent}`;
}

/**
 * Where the keyboard goes when the drawer closes. A Control view can cover
 * Crew, and a hidden row cannot take focus, so the agent's row is the target
 * only while Crew is open and no view is.
 */
export function drawerHandBack(state: {
  readonly agent: string | null;
  readonly focused: boolean;
  readonly crewOpen: boolean;
  readonly viewOpen: boolean;
}): string {
  if (state.focused) return 'focus-toggle';
  return state.crewOpen && !state.viewOpen && state.agent !== null ? crewRowId(state.agent) : 'crew-toggle';
}

/**
 * Every current agent as a list a keyboard or a screen reader can use on its
 * own. It reads only the projection, never the office, so it also stands in
 * for the office when the office cannot draw.
 */
export function CrewList({ agents, cards = [], selectedId, selectable, stale, onSelectAgent, onOpenCard, addAgent }: {
  readonly agents: readonly AgentRow[];
  readonly cards?: readonly CardRow[];
  readonly selectedId: string | null;
  readonly selectable: boolean;
  readonly stale: boolean;
  readonly onSelectAgent: (id: string) => void;
  readonly onOpenCard: (id: string) => void;
  readonly addAgent?: ReactNode;
}) {
  const { ordered, move } = useAgentOrder(agents, 'strip');
  const ids = ordered.map((row) => row.id);
  const buttons = useRef(new Map<string, HTMLButtonElement>());
  useLiveActivityExpiry(agents, selectable && !stale);
  const now = performance.now();
  const checkouts = checkoutSummary(agents);

  function reveal(id: string): void {
    buttons.current.get(id)?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  }

  useEffect(() => {
    if (selectedId !== null) reveal(selectedId);
  }, [selectedId]);

  return (
    <section className="crew-list" id="crew-list" aria-labelledby="crew-title" data-stale={stale ? 'true' : 'false'}>
      <h2 id="crew-title">Crew <span>{agents.length}</span></h2>
      {checkouts === null ? null : <p className="crew-checkouts">Checkouts: {checkouts}</p>}
      {agents.length === 0 ? <p className="crew-empty">No current agents.</p> : <ul className="crew-rows">
        {ordered.map((row) => {
          const crew = crewRowOf(row, cards, { live: selectable && !stale, now, wallNow: Date.now() });
          const about = `crew-about-${row.id}`;
          return (
            <li className="crew-entry" key={row.id} data-reorder-agent={row.id} data-reorder-scope="strip">
              <ReorderHandle agent={row.id} name={crew.name} ids={ids} scope="strip" onMove={move} />
              <button
                ref={(button) => {
                  if (button === null) buttons.current.delete(row.id);
                  else buttons.current.set(row.id, button);
                }}
                type="button"
                id={crewRowId(row.id)}
                className="crew-row"
                data-agent-id={row.id}
                aria-label={`Inspect ${crew.name} in Crew`}
                aria-describedby={about}
                aria-pressed={selectedId === row.id}
                aria-controls={selectedId === row.id ? 'selected-agent-detail' : undefined}
                disabled={!selectable}
                onFocus={() => reveal(row.id)}
                onClick={() => onSelectAgent(row.id)}
              >
                <span className="crew-dot" aria-hidden="true"
                  style={{ '--agent': crew.colour } as CSSProperties}>{initialOf(crew.name)}</span>
                <span className="crew-who">
                  <strong className="crew-name">{crew.name}</strong>
                  <small className="crew-meta">{crew.role} · {crew.engine}</small>
                  <small className="crew-workspace" title={crew.workspace}
                    data-recorded={decodeWorkspace(row.workspace) !== null}>{crew.workspace}</small>
                  {crew.checkout.worktree === null ? null : <small className="crew-checkout" data-checkout="worktree"
                    title={crew.checkout.worktree}>{crew.checkout.worktree}</small>}
                  {!crew.checkout.shared ? null : <small className="crew-checkout" data-checkout="shared"
                    title="Another live agent works in this directory">{SHARED_DIRECTORY_TEXT}</small>}
                  <small className="crew-activity" data-activity={crew.activity.kind}
                    title={crew.activity.text}>{crew.activity.line}</small>
                  {crew.liveActivity === null ? null : <small className="crew-live-activity"
                    data-live-activity={crew.liveActivity}>{crew.liveActivity}</small>}
                </span>
                <span className="crew-state">
                  <span className="crew-chip" data-status={crew.status}>{crew.status}</span>
                  {crew.hold === 'paused' || crew.hold === 'halted' ? <span className="crew-hold">{crew.hold}</span> : null}
                </span>
              </button>
              {crew.activeCard === null ? null : <button type="button" className="crew-card-note"
                aria-label={`Open card ${crew.activeCard.title} for ${crew.name}`}
                aria-controls="selected-card-detail" title={crew.activeCard.title} disabled={!selectable}
                onFocus={() => reveal(row.id)} onClick={() => {
                  if (crew.activeCard !== null) onOpenCard(crew.activeCard.id);
                }}>Card: {crew.activeCard.title}</button>}
              <span id={about} hidden>{crew.description}</span>
            </li>
          );
        })}
      </ul>}
      {addAgent}
    </section>
  );
}
