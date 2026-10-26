// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, useState } from 'react';
import type { CSSProperties, ReactNode } from 'react';

import { Badge, Button, cn, RING_INSET } from '../ui/index.ts';
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

/** Below 800 pixels the side roster is a bottom sheet that opens from its heading. */
const SHEET = cn(
  'max-[800px]:fixed max-[800px]:inset-x-0 max-[800px]:top-auto max-[800px]:bottom-0 max-[800px]:z-[var(--layer-top-bar)]',
  'max-[800px]:m-0 max-[800px]:grid max-[800px]:max-h-[70vh] max-[800px]:w-auto max-[800px]:max-w-none',
  'max-[800px]:grid-cols-[minmax(0,1fr)_auto] max-[800px]:content-start max-[800px]:items-center max-[800px]:gap-2',
  'max-[800px]:rounded-t-xl max-[800px]:border-x-0 max-[800px]:border-b-0 max-[800px]:border-t max-[800px]:bg-surface',
  'max-[800px]:p-3 max-[800px]:shadow-sheet',
);

const ROWS = 'crew-rows m-0 grid list-none gap-1 p-0';

/** A row whose agent has something for the operator carries its state in its edge and wash, so it reads before its words do. */
const ENTRY_STATE: Readonly<Record<NeedState, string>> = {
  'needs-you': 'border-warn/40 bg-warn-soft',
  ready: 'border-accent/40 bg-accent-soft',
  working: '', waiting: '', done: '', idle: '',
};

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

const STAGE_FRAME = cn('z-(--layer-panels) m-0 box-border min-w-0 overflow-auto border-0 border-solid border-line bg-surface text-ink',
  'min-[801px]:col-1 min-[801px]:row-[2/-1] min-[801px]:flex min-[801px]:min-h-0 min-[801px]:w-[min(264px,40vw)] min-[801px]:flex-col min-[801px]:gap-2 min-[801px]:border-r min-[801px]:px-2 min-[801px]:py-4');

/**
 * Every current agent, one row each, ordered by who needs the operator first. A row says who, in one state
 * word what is going on, what the agent is doing, and offers the one click that acts on it. It reads only
 * the projection, never the office, so it also stands in for the office when the office cannot draw.
 */
export function Roster({ agents, cards = [], stepLabels = {}, selectedId, selectable, stale, onSelectAgent, onOpenQuestion,
  onOpenTicket, addAgent, base = 'crew', order = 'strip', sheet = false, hidden = false }: {
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
  /** Leaves the roster mounted but out of the page, while a Control view takes the stage. */
  readonly hidden?: boolean;
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
    const selected = selectedId === row.id;
    return (
      <li className={cn('crew-entry roster-entry flex items-center gap-1 rounded-lg border border-transparent pr-1 transition-[background-color,border-color]',
        'hover:bg-surface-2 data-[selected=true]:border-accent data-[selected=true]:bg-accent-soft', ENTRY_STATE[row.state])}
      key={row.id} data-state={row.state} data-selected={selected} data-reorder-agent={row.id}
      data-reorder-scope={order} style={{ '--agent': crew.colour } as CSSProperties}>
        <button
          ref={(button) => {
            if (button === null) buttons.current.delete(row.id);
            else buttons.current.set(row.id, button);
          }}
          type="button"
          id={crewRowId(row.id, base)}
          className={cn('crew-row m-0 flex min-w-0 flex-1 cursor-pointer items-center gap-3 rounded-lg border-0 bg-transparent p-2 text-left',
            'font-sans text-base text-ink disabled:cursor-default', RING_INSET)}
          data-agent-id={row.id}
          aria-label={`Inspect ${crew.name}`}
          aria-describedby={about}
          aria-pressed={selected}
          aria-controls={selected ? 'selected-agent-detail' : undefined}
          disabled={!selectable}
          onFocus={() => reveal(row.id)}
          onClick={() => onSelectAgent(row.id)}
        >
          {agent === undefined ? null
            : <Portrait className={cn('roster-face size-9 shrink-0 self-start rounded-full data-[portrait=figure]:bg-surface-2',
              'data-[portrait=badge]:text-title data-[portrait=badge]:leading-9 data-[portrait=badge]:font-semibold',
              'shadow-[inset_0_0_0_2px_var(--agent)]')}
            look={lookOf(agent)} accessory={accessoryOf(agent.role)} name={crew.name} />}
          <span className="crew-who grid min-w-0 flex-1 gap-0.5">
            <span className="roster-line flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5">
              <strong className="crew-name truncate text-base font-semibold"><bdi>{crew.name}</bdi></strong>
              <Badge state={row.state} className="roster-state"><span className="visually-hidden">Status: </span>{NEED_LABEL[row.state]}</Badge>
            </span>
            <small className="crew-activity truncate text-small text-muted data-[activity=question]:font-medium data-[activity=question]:text-ink"
            data-activity={crew.activity.kind} title={row.doing}>
              {row.live === null ? null : <span className="crew-live-activity after:content-['_·_']" data-live-activity={row.live}>{row.live}</span>}
              {row.doing}
            </small>
          </span>
        </button>
        <span className="roster-acts flex flex-col gap-1">
          <Button size="sm" variant={row.action.kind === 'agent' ? 'secondary' : 'primary'} className={cn('roster-act', 'max-[800px]:h-10')}
            data-primary={row.action.kind !== 'agent'} disabled={!selectable}
            aria-label={`${row.action.label}, ${crew.name}`} onClick={() => act(row.action, row.id)}>{row.action.label}</Button>
          {row.second === null ? null : <Button size="sm" variant="primary" className={cn('roster-act', 'max-[800px]:h-10')}
            data-primary="true" disabled={!selectable}
            aria-label={`${row.second.label}, ${crew.name}`} onClick={() => { if (row.second !== null) act(row.second, row.id); }}>
            {row.second.label}</Button>}
        </span>
        <ReorderHandle agent={row.id} name={crew.name} ids={ids} scope={order} onMove={move} />
        <span id={about} hidden>{describe(row)}</span>
      </li>
    );
  }

  const groups: readonly (readonly [NeedState, readonly Placed[]])[] = !grouped ? [] : NEED_STATES
    .map((state) => [state, placed.filter((place) => place.group === state)] as const)
    .filter(([, members]) => members.length > 0);

  const waiting = rows.some((row) => row.state === 'needs-you' || row.state === 'ready');
  const sheeted = sheet && !open;
  return (
    <section className={cn('crew-list roster font-sans text-ink', hidden ? 'hidden' : sheet ? [STAGE_FRAME, SHEET] : 'flex flex-col gap-2')}
    id={`${base}-list`} aria-labelledby={title} data-stale={stale ? 'true' : 'false'}
    data-sheet={sheet ? (open ? 'open' : 'closed') : undefined} data-waiting={waiting}>
      <h2 id={title} className={cn('roster-title m-0 text-body font-semibold', !waiting && 'text-muted')}>
        {agents.length === 0 ? 'No agents yet' : waitingHeading(rows)}</h2>
      {sheet ? <Button size="md" className="roster-sheet-toggle hidden max-[800px]:inline-flex max-[800px]:h-10" aria-expanded={open} aria-controls={list}
        onClick={() => setOpen((current) => !current)}>{open ? 'Hide agents' : `Show ${agents.length} ${agents.length === 1 ? 'agent' : 'agents'}`}</Button> : null}
      <div className={cn('roster-body flex flex-col gap-2', sheet && 'max-[800px]:col-span-full', sheeted && 'max-[800px]:hidden')} id={list} ref={attach}>
        {agents.length === 0 ? <p className="crew-empty m-0 px-1 text-base text-muted">No current agents.</p>
          : grouped ? groups.map(([state, members]) => <section key={state} className="roster-group" aria-labelledby={`${base}-group-${state}`}>
            <h3 id={`${base}-group-${state}`} className="m-0 mt-2 mb-1 px-1 text-caption font-semibold tracking-label text-muted uppercase">
              {NEED_LABEL[state]} <span className="ml-1 font-normal tabular-nums">{members.length}</span></h3>
            <ul className={ROWS} aria-label={`${NEED_LABEL[state]} agents`}>{members.map(entry)}</ul>
          </section>)
            : <ul className={ROWS} aria-label="Agents">{placed.map(entry)}</ul>}
        {addAgent}
      </div>
    </section>
  );
}
