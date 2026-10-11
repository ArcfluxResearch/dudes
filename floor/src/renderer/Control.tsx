// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ReactNode } from 'react';
import { useState } from 'react';

import { ArchivedAgents } from './ArchivedAgents.tsx';
import type { RetiredAgentRow } from './archive.ts';
import { LogPanel } from './LogPanel.tsx';
import { ReorderHandle, useAgentOrder } from './AgentOrder.tsx';
import { journalPosition } from './format.ts';
import { closesPanel, handFocusOnLeave } from './focus.ts';
import { StreamReadStatus } from './ReadError.tsx';
import type { FloorState } from './floor/store.ts';
import { waitingOnYou } from './questions.ts';
import type { WaitingItem } from './questions.ts';
import { fieldsOf } from './scene/label.ts';
import { SetupView } from './setup/SetupView.tsx';
import { SettingsView } from './settings/SettingsView.tsx';
import { Board, CardDetail } from './BoardPanel.tsx';
import { drawsTickets, TicketBoard } from './ticket/TicketBoard.tsx';
import { TicketPanel } from './ticket/TicketPanel.tsx';
import { workspaceGroups, workspaceLabel } from './workspace.ts';
import { HireEntry } from './onboarding/HireEntry.tsx';

export type ControlView = 'overview' | 'board' | 'log' | 'setup' | 'settings';

/** Control's views, in top bar order. Setup and Settings take a fresh snapshot when they open. */
export const CONTROL_VIEWS: readonly { readonly view: ControlView; readonly label: string; readonly refresh: boolean }[] = [
  { view: 'overview', label: 'Overview', refresh: false },
  { view: 'board', label: 'Board', refresh: false },
  { view: 'log', label: 'Log', refresh: false },
  { view: 'setup', label: 'Setup', refresh: true },
  { view: 'settings', label: 'Settings', refresh: true },
];

export const CONTROL_TITLE_ID = 'control-title';

/** A view button's target: no view when that view is already open beside the office, else the view itself. */
export function toggledView(open: ControlView | null, pressed: ControlView, focused: boolean): ControlView | null {
  return open === pressed && !focused ? null : pressed;
}

export function viewButtonId(view: ControlView): string {
  return `view-${view}`;
}

function labelOf(view: ControlView): string {
  return CONTROL_VIEWS.find((entry) => entry.view === view)?.label ?? view;
}

function value(value: unknown): string {
  return typeof value === 'string' && value !== '' ? value : '—';
}

function focusOnArrival(node: HTMLHeadingElement | null): void {
  node?.focus();
}

export function OverviewSummary({ floor, selectedId, onOpenQuestion, onOpenTicket, onHireAgain }: {
  readonly floor: FloorState;
  readonly selectedId: string | null;
  readonly onOpenQuestion: (agentId: string) => void;
  readonly onOpenTicket?: (card: string) => void;
  readonly onHireAgain?: (row: RetiredAgentRow, opener: HTMLButtonElement) => void;
}) {
  return (
    <div className="overview-summary">
      <StreamReadStatus floor={floor} />
      <WaitingSection floor={floor} onOpen={onOpenQuestion} onOpenTicket={onOpenTicket} />
      <dl>
        <div><dt>Current agents</dt><dd>{floor.agents.length}</dd></div>
        <div><dt>Journal position</dt><dd className="data">{journalPosition(floor.lsn)}</dd></div>
      </dl>
      <p>{selectedId === null
        ? 'Select an agent in the office or in Crew to open its details.'
        : 'The selected agent’s current projection and read-only Live work are open in its details.'}</p>
      <ArchivedAgents floor={floor} onHireAgain={onHireAgain} />
    </div>
  );
}

/** Focus mode's roster: every current agent by workspace, in this browser's own order. */
export function AgentsRoster({ floor, selectedId, onSelectAgent, onHireAgain, onAddAgent }: {
  readonly floor: FloorState;
  readonly selectedId: string | null;
  readonly onSelectAgent: (id: string) => void;
  readonly onHireAgain?: (row: RetiredAgentRow, opener: HTMLButtonElement) => void;
  readonly onAddAgent?: (opener: HTMLButtonElement) => void;
}) {
  const selectable = floor.phase === 'live' && !floor.stale;
  const { ordered, move } = useAgentOrder(floor.agents, 'focus');
  const groups = workspaceGroups(ordered);
  return (
    <div className="agents-roster">
      <StreamReadStatus floor={floor} />
      {floor.agents.length === 0 ? <p>No current agents.</p> : (
        groups.map((group, at) => <section key={group.id === null ? 'unrecorded' : `workspace-${group.id}`}
          className="workspace-roster-group" aria-labelledby={`workspace-roster-${at}`}>
          <h3 id={`workspace-roster-${at}`}>{group.label}</h3>
          {group.labelsDiffer ? <p className="workspace-roster-note">This workspace has agents with different labels.</p> : null}
          <ul aria-label={`Agents in ${group.label}`}>
            {group.agents.map((row) => {
              const fields = fieldsOf(row);
              return (
                <li key={row.id} data-reorder-agent={row.id} data-reorder-scope={`focus-${group.id ?? 'unrecorded'}`}>
                  <ReorderHandle agent={row.id} name={value(row.name)}
                    ids={group.agents.map((agent) => agent.id)} scope={`focus-${group.id ?? 'unrecorded'}`} onMove={move} />
                  <button type="button" className="agents-roster-row" data-agent-id={row.id}
                    aria-label={`Inspect ${value(row.name)} in focus`}
                    aria-pressed={selectedId === row.id}
                    aria-controls={selectedId === row.id ? 'selected-agent-detail' : undefined}
                    disabled={!selectable} onClick={() => onSelectAgent(row.id)}>
                    <strong>{value(row.name)}</strong>
                    <span>{value(row.role)} · {value(row.provider)}</span>
                    <span className="agents-roster-workspace">{workspaceLabel(row)}</span>
                    <span>{value(fields.status)}</span>
                    <span>{fields.goal === '' ? 'No goal' : fields.goal}</span>
                    <span>{fields.cost}</span>
                  </button>
                </li>
              );
            })}
          </ul>
        </section>)
      )}
      {onAddAgent === undefined ? null : <HireEntry floor={floor} id="hire-open-focus" onAddAgent={onAddAgent} />}
      <ArchivedAgents floor={floor} onHireAgain={onHireAgain} />
    </div>
  );
}

export function FocusRoster({ collapsed, onToggle, ...roster }: Parameters<typeof AgentsRoster>[0] & {
  readonly collapsed: boolean;
  readonly onToggle: () => void;
}) {
  return (
    <section className="focus-roster" aria-label="Focus roster">
      <button type="button" className="focus-roster-toggle" aria-expanded={!collapsed}
        aria-controls="focus-roster-list" onClick={onToggle}>
        {collapsed ? 'Show roster' : 'Hide roster'}
      </button>
      {collapsed ? null : <div id="focus-roster-list"><AgentsRoster {...roster} /></div>}
    </section>
  );
}

/** The things waiting on the operator, one entry each. The list is absent when nothing waits. */
export function WaitingOnYou({ items, selectable, onOpen, onOpenTicket }: {
  readonly items: readonly WaitingItem[];
  readonly selectable: boolean;
  readonly onOpen: (agentId: string) => void;
  readonly onOpenTicket?: (card: string) => void;
}) {
  return (
    <section className="waiting-on-you" aria-labelledby="waiting-on-you-title">
      <h3 id="waiting-on-you-title">Waiting on you</h3>
      <ol aria-label="Things waiting on you">
        {items.map((item) => (
          <li key={item.key} className="waiting-item" data-waiting-kind={item.kind}
            data-waiting-agent-id={item.agentId ?? undefined} data-waiting-card-id={item.kind === 'ticket' ? item.card : undefined}>
            <h4>{item.name}</h4>
            <p>{item.text}</p>
            {item.kind === 'question'
              ? <button type="button" className="waiting-open" disabled={!selectable}
                onClick={() => onOpen(item.agentId)}>Answer</button>
              : <button type="button" className="waiting-open" disabled={!selectable || onOpenTicket === undefined}
                onClick={() => onOpenTicket?.(item.card)}>Open ticket</button>}
          </li>
        ))}
      </ol>
    </section>
  );
}

function WaitingSection({ floor, onOpen, onOpenTicket }: {
  readonly floor: FloorState;
  readonly onOpen: (agentId: string) => void;
  readonly onOpenTicket?: (card: string) => void;
}) {
  const items = waitingOnYou(floor.agents, floor.cards);
  return items.length === 0 ? null
    : <WaitingOnYou items={items} selectable={floor.phase === 'live' && !floor.stale} onOpen={onOpen}
      onOpenTicket={onOpenTicket} />;
}

export function ControlNav({ floor, view, onToggle }: {
  readonly floor: FloorState;
  readonly view: ControlView | null;
  readonly onToggle: (view: ControlView) => void;
}) {
  return (
    <nav className="control-nav" aria-label="Views">
      {CONTROL_VIEWS.map((entry) => <button key={entry.view} type="button" id={viewButtonId(entry.view)}
        aria-pressed={view === entry.view} aria-controls={view === entry.view ? 'control-panel' : undefined}
        onClick={() => onToggle(entry.view)}>
        {entry.label}
      </button>)}
    </nav>
  );
}

export function Control({ floor, view, selectedId, onClose, onView, onOpenQuestion, onOpenTicket, onHire, onHireAgain, onGuide }: {
  readonly floor: FloorState;
  readonly view: ControlView;
  readonly selectedId: string | null;
  readonly onClose: () => void;
  readonly onView: (view: ControlView) => void;
  readonly onOpenQuestion: (agentId: string) => void;
  /** Open the Board on a ticket. */
  readonly onOpenTicket?: (card: string) => void;
  /** Open the Hire form, from the new-ticket form when no agent is hired. */
  readonly onHire?: (opener: HTMLElement | null) => void;
  readonly onHireAgain?: (row: RetiredAgentRow, opener: HTMLButtonElement) => void;
  readonly onGuide?: () => void;
}) {
  const [selectedCardId, setSelectedCardId] = useState<string | null>(null);
  const selectedCard = (floor.cards ?? []).find((card) => card.id === selectedCardId) ?? null;
  const panels: Record<ControlView, ReactNode> = {
    overview: <OverviewSummary floor={floor} selectedId={selectedId} onOpenQuestion={onOpenQuestion}
      onOpenTicket={onOpenTicket} onHireAgain={onHireAgain} />,
    board: drawsTickets(floor)
      ? <TicketBoard floor={floor} selectedId={selectedCardId} onSelect={setSelectedCardId} onHire={onHire} />
      : <Board floor={floor} selectedId={selectedCardId} onSelect={setSelectedCardId} />,
    log: <LogPanel floor={floor} />,
    setup: <SetupView floor={floor} onGuide={onGuide} />,
    settings: <SettingsView floor={floor} onSetup={() => onView('setup')} />,
  };
  const label = labelOf(view);
  return (
    <section className="control-panel" id="control-panel" data-view={view} aria-labelledby={CONTROL_TITLE_ID}
      ref={handFocusOnLeave} data-focus-on-leave={viewButtonId(view)}
      onKeyDown={(event) => { if (closesPanel(event)) { event.stopPropagation(); onClose(); } }}>
      <header className="control-panel-head">
        <h2 id={CONTROL_TITLE_ID} tabIndex={-1} ref={focusOnArrival}>{label}</h2>
        <button type="button" className="panel-close" aria-label={`Close ${label}`} title="Close (Escape)"
          onClick={onClose}>×</button>
      </header>
      <div className="control-view">{panels[view]}</div>
      {view === 'board' ? <div className="control-detail" id="selected-card-detail" key={selectedCardId ?? 'card-none'}
        ref={handFocusOnLeave} data-focus-on-leave={viewButtonId('board')}>
        {selectedCard?.ticket != null ? <TicketPanel floor={floor} card={selectedCard} onHire={onHire} />
          : <CardDetail floor={floor} card={selectedCard} />}
      </div> : null}
    </section>
  );
}
