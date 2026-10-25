// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, useState } from 'react';
import type { CSSProperties, ReactNode } from 'react';

import { accessoryOf, lookOf } from './appearance.ts';
import { ReorderHandle, useAgentOrder } from './AgentOrder.tsx';
import type { CardRow } from './board.ts';
import { NEED_LABEL, NEED_STATES, needsOf, waitingHeading } from './needs.ts';
import type { NeedAction, NeedRow, NeedState } from './needs.ts';
import { Portrait } from './Portrait.tsx';
import type { AgentRow } from './scene/cast.ts';
import type { AgentOrderView } from './view.ts';
import { useHeldLayout } from './useHeldLayout.ts';
import type { Placed } from './useHeldLayout.ts';
import { useLiveActivityExpiry } from './useLiveActivityExpiry.ts';

export const GROUP_FROM = 30;

export function crewRowId(agent: string, base = 'crew'): string {
  return `${base}-row-${agent}`;
}

/**
 * Where the keyboard goes when the drawer closes. A Control view can cover
 * the roster, and a hidden row cannot take focus, so the agent's row is the target
 * only while the roster is open and no view is.
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

function describe(row: NeedRow): string {
  const { crew } = row;
  const facts = [crew.role, crew.engine].filter((fact) => fact !== '—').join(', ');
  const live = row.live === null ? '' : `. ${row.live}`;
  return `${NEED_LABEL[row.state]}. ${facts === '' ? '' : `${facts}. `}${row.doing}${live}. Project: ${crew.workspace}`;
}

/**
 * Every current agent, one row each, ordered by who needs the operator first. A row says who, in one state
 * word what is going on, what the agent is doing, and offers the one click that acts on it. It reads only
 * the projection, never the office, so it also stands in for the office when the office cannot draw.
 */
export function Roster({ agents, cards = [], stepLabels = {}, selectedId, selectable, stale, onSelectAgent, onOpenQuestion,
  onOpenTicket, addAgent, base = 'crew', order = 'strip', sheet = false }: {
  readonly agents: readonly AgentRow[];
  readonly stepLabels?: Readonly<Record<string, string>>;
  readonly cards?: readonly CardRow[];
  readonly selectedId: string | null;
  readonly selectable: boolean;
  readonly stale: boolean;
  readonly onSelectAgent: (id: string) => void;
  readonly onOpenQuestion: (id: string) => void;
  readonly onOpenTicket: (id: string) => void;
  readonly addAgent?: ReactNode;
  /** The prefix of every id the roster sets, so two rosters on one page never share one. */
  readonly base?: string;
  /** Whose own order the rows follow within a state: this browser keeps one per place the roster appears. */
  readonly order?: AgentOrderView;
  /** Below 800 pixels the side roster is a bottom sheet that opens from its heading. */
  readonly sheet?: boolean;
}) {
  const { ordered, move } = useAgentOrder(agents, order);
  const live = selectable && !stale;
  useLiveActivityExpiry(agents, live);
  const rows = needsOf(ordered, cards, { live, now: performance.now(), wallNow: Date.now() }, stepLabels);
  const { attach, placed } = useHeldLayout(rows);
  const [open, setOpen] = useState(false);
  const buttons = useRef(new Map<string, HTMLButtonElement>());
  const byId = new Map(agents.map((row) => [row.id, row] as const));
  const grouped = agents.length >= GROUP_FROM;
  const ids = placed.map(({ row }) => row.id);
  const title = `${base}-title`;
  const list = `${base}-rows`;

  function reveal(id: string): void {
    buttons.current.get(id)?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
  }

  useEffect(() => {
    if (selectedId !== null) reveal(selectedId);
  }, [selectedId]);

  function act(action: NeedAction, agent: string): void {
    if (action.kind === 'question') onOpenQuestion(agent);
    else if (action.kind === 'ticket') onOpenTicket(action.card);
    else onSelectAgent(agent);
  }

  function entry({ row }: Placed): ReactNode {
    const agent = byId.get(row.id);
    const { crew } = row;
    const about = `${base}-about-${row.id}`;
    return (
      <li className="crew-entry roster-entry" key={row.id} data-state={row.state} data-reorder-agent={row.id}
        data-reorder-scope={order} style={{ '--agent': crew.colour } as CSSProperties}>
        <button
          ref={(button) => {
            if (button === null) buttons.current.delete(row.id);
            else buttons.current.set(row.id, button);
          }}
          type="button"
          id={crewRowId(row.id, base)}
          className="crew-row"
          data-agent-id={row.id}
          aria-label={`Inspect ${crew.name}`}
          aria-describedby={about}
          aria-pressed={selectedId === row.id}
          aria-controls={selectedId === row.id ? 'selected-agent-detail' : undefined}
          disabled={!selectable}
          onFocus={() => reveal(row.id)}
          onClick={() => onSelectAgent(row.id)}
        >
          {agent === undefined ? null
            : <Portrait className="roster-face" look={lookOf(agent)} accessory={accessoryOf(agent.role)} name={crew.name} />}
          <span className="crew-who">
            <span className="roster-line">
              <strong className="crew-name"><bdi>{crew.name}</bdi></strong>
              <span className="crew-chip" data-state={row.state}><span className="visually-hidden">Status: </span>{NEED_LABEL[row.state]}</span>
            </span>
            <small className="crew-activity" data-activity={crew.activity.kind} title={row.doing}>
              {row.live === null ? null : <span className="crew-live-activity" data-live-activity={row.live}>{row.live}</span>}
              {row.doing}
            </small>
          </span>
        </button>
        <span className="roster-acts">
          <button type="button" className="roster-act" data-primary={row.action.kind !== 'agent'} disabled={!selectable}
            aria-label={`${row.action.label}, ${crew.name}`} onClick={() => act(row.action, row.id)}>{row.action.label}</button>
          {row.second === null ? null : <button type="button" className="roster-act" data-primary="true" disabled={!selectable}
            aria-label={`${row.second.label}, ${crew.name}`} onClick={() => { if (row.second !== null) act(row.second, row.id); }}>
            {row.second.label}</button>}
        </span>
        <ReorderHandle agent={row.id} name={crew.name} ids={ids} scope={order} onMove={move} />
        <span id={about} hidden>{describe(row)}</span>
      </li>
    );
  }

  const groups: readonly (readonly [NeedState, readonly Placed[]])[] = !grouped ? [] : NEED_STATES
    .map((state) => [state, placed.filter((place) => place.group === state)] as const)
    .filter(([, members]) => members.length > 0);

  return (
    <section className="crew-list roster" id={`${base}-list`} aria-labelledby={title} data-stale={stale ? 'true' : 'false'}
      data-sheet={sheet ? (open ? 'open' : 'closed') : undefined} data-waiting={rows.some((row) => row.state === 'needs-you' || row.state === 'ready')}>
      <h2 id={title} className="roster-title">{agents.length === 0 ? 'No agents yet' : waitingHeading(rows)}</h2>
      {sheet ? <button type="button" className="roster-sheet-toggle" aria-expanded={open} aria-controls={list}
        onClick={() => setOpen((current) => !current)}>{open ? 'Hide agents' : `Show ${agents.length} ${agents.length === 1 ? 'agent' : 'agents'}`}</button> : null}
      <div className="roster-body" id={list} ref={attach}>
        {agents.length === 0 ? <p className="crew-empty">No current agents.</p>
          : grouped ? groups.map(([state, members]) => <section key={state} className="roster-group" aria-labelledby={`${base}-group-${state}`}>
            <h3 id={`${base}-group-${state}`}>{NEED_LABEL[state]} <span>{members.length}</span></h3>
            <ul className="crew-rows" aria-label={`${NEED_LABEL[state]} agents`}>{members.map(entry)}</ul>
          </section>)
            : <ul className="crew-rows" aria-label="Agents">{placed.map(entry)}</ul>}
        {addAgent}
      </div>
    </section>
  );
}
