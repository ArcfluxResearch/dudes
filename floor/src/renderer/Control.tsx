// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ReactNode } from 'react';
import { useEffect, useRef, useState } from 'react';

import { ArchivedAgents } from './ArchivedAgents.tsx';
import type { RetiredAgentRow } from './archive.ts';
import { LogPanel } from './LogPanel.tsx';
import { Roster } from './Roster.tsx';
import { closesPanel, handFocusOnLeave, restoreFocusAfterDialog } from './focus.ts';
import { StreamReadStatus } from './ReadError.tsx';
import type { FloorState } from './floor/store.ts';
import { SetupView } from './setup/SetupView.tsx';
import { SettingsView } from './settings/SettingsView.tsx';
import { CardDetail } from './BoardPanel.tsx';
import { NewTicketForm, TicketBoard, TicketNotice } from './ticket/TicketBoard.tsx';
import { viewConfirmed } from './ticket/model.ts';
import { TicketOffHint } from './ticket/TicketGuidance.tsx';
import { TicketPanel } from './ticket/TicketPanel.tsx';
import { HireEntry } from './onboarding/HireEntry.tsx';
import { isPageView } from '../navigation.ts';
import { Button, ICON, IconButton, cn } from '../ui/index.ts';
import { X } from 'lucide-react';

export type ControlView = 'overview' | 'board' | 'log' | 'setup' | 'settings';

export type PanelView = Exclude<ControlView, 'overview'>;

/** Control's views, in top bar order. Setup and Settings take a fresh snapshot when they open. */
export const CONTROL_VIEWS: readonly { readonly view: ControlView; readonly label: string; readonly refresh: boolean }[] = [
  { view: 'overview', label: 'Overview', refresh: false },
  { view: 'board', label: 'Board', refresh: false },
  { view: 'log', label: 'Log', refresh: false },
  { view: 'setup', label: 'Setup', refresh: true },
  { view: 'settings', label: 'Settings', refresh: true },
];

export const CONTROL_TITLE_ID = 'control-title';

type BoardOverlay = { readonly kind: 'new' } | { readonly kind: 'card'; readonly id: string } | null;

export function toggledView(open: ControlView | null, pressed: ControlView, focused: boolean): ControlView | null {
  return open === pressed && !focused ? null : pressed;
}

export function viewButtonId(view: ControlView): string {
  return `view-${view}`;
}

function labelOf(view: ControlView): string {
  return CONTROL_VIEWS.find((entry) => entry.view === view)?.label ?? view;
}

function focusOnArrival(node: HTMLHeadingElement | null): void {
  node?.focus();
}

export function AgentsRoster({ floor, selectedId, onSelectAgent, onOpenQuestion, onOpenTicket, onHireAgain, onAddAgent }: {
  readonly floor: FloorState;
  readonly selectedId: string | null;
  readonly onSelectAgent: (id: string) => void;
  readonly onOpenQuestion: (id: string) => void;
  readonly onOpenTicket: (card: string) => void;
  readonly onHireAgain?: (row: RetiredAgentRow, opener: HTMLButtonElement) => void;
  readonly onAddAgent?: (opener: HTMLButtonElement) => void;
}) {
  return (
    <div className="agents-roster">
      <StreamReadStatus floor={floor} />
      <Roster base="focus" order="focus" agents={floor.agents} cards={floor.cards} stepLabels={floor.stepLabels}
        selectedId={selectedId} selectable={floor.phase === 'live' && !floor.stale} stale={floor.stale}
        onSelectAgent={onSelectAgent} onOpenQuestion={onOpenQuestion} onOpenTicket={onOpenTicket} />
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
    <section className="focus-roster grid min-h-0 content-start gap-3 overflow-auto max-[800px]:order-first max-[800px]:max-h-56" aria-label="Focus roster">
      <Button className="focus-roster-toggle w-full" aria-expanded={!collapsed}
        aria-controls="focus-roster-list" onClick={onToggle}>
        {collapsed ? 'Show roster' : 'Hide roster'}
      </Button>
      {collapsed ? null : <div id="focus-roster-list"><AgentsRoster {...roster} /></div>}
    </section>
  );
}

export function ControlNav({ floor, view, onToggle }: {
  readonly floor: FloorState;
  readonly view: ControlView | null;
  readonly onToggle: (view: ControlView) => void;
}) {
  return (
    <nav className="control-nav flex flex-wrap items-center gap-1" aria-label="Views">
      {CONTROL_VIEWS.map((entry) => <Button key={entry.view} variant="ghost" id={viewButtonId(entry.view)}
        className={cn(view === entry.view && 'bg-accent-soft text-accent hover:bg-accent-soft hover:text-accent')}
        aria-pressed={view === entry.view}
        aria-controls={view === entry.view ? entry.view === 'overview' ? 'overview-dialog' : 'control-panel' : undefined}
        onClick={() => onToggle(entry.view)}>
        {entry.label}
      </Button>)}
    </nav>
  );
}

export function Control({ floor, view, onClose, onView, onReviewRecovery, onHire, onGuide }: {
  readonly floor: FloorState;
  readonly view: PanelView;
  readonly onClose: () => void;
  readonly onView: (view: ControlView) => void;
  /** Close the ticket and open the agent's drawer on its recovery view. */
  readonly onReviewRecovery?: (agent: string) => void;
  /** Open the Hire form, from the new-ticket form when no agent is hired. */
  readonly onHire?: (opener: HTMLElement | null) => void;
  readonly onGuide?: () => void;
}) {
  const [overlay, setOverlay] = useState<BoardOverlay>(null);
  const selectedCardId = overlay?.kind === 'card' ? overlay.id : null;
  const selectedCard = (floor.cards ?? []).find((card) => card.id === selectedCardId) ?? null;
  const opener = useRef<HTMLElement | null>(null);
  const dialog = useRef<HTMLDialogElement>(null);
  const seenCard = useRef<string | null>(null);
  const showing = view === 'board' && overlay !== null;

  function openCard(id: string, from?: HTMLElement): void {
    opener.current = from ?? document.querySelector<HTMLButtonElement>(`[data-card-id="${id}"]`);
    seenCard.current = null;
    setOverlay({ kind: 'card', id });
  }

  function openNewTicket(from: HTMLElement): void {
    opener.current = from;
    seenCard.current = null;
    setOverlay({ kind: 'new' });
  }

  function closeOverlay(): void {
    const replacement = selectedCardId === null ? null
      : dialog.current?.ownerDocument.querySelector<HTMLButtonElement>(`[data-card-id="${selectedCardId}"]:not(:disabled)`) ?? null;
    const fallback = replacement ?? document.getElementById(CONTROL_TITLE_ID) ?? document.getElementById(viewButtonId('board'));
    dialog.current?.close();
    setOverlay(null);
    restoreFocusAfterDialog(opener.current, fallback);
  }

  useEffect(() => {
    if (overlay?.kind !== 'card') return;
    if (selectedCard !== null) seenCard.current = selectedCard.id;
    else if (seenCard.current === overlay.id && viewConfirmed(floor)) closeOverlay();
  }, [selectedCardId, selectedCard, floor]);

  useEffect(() => {
    if (!showing) return undefined;
    const node = dialog.current;
    node?.showModal();
    (overlay?.kind === 'new' ? node?.querySelector<HTMLElement>('input[name="title"]')
      : node?.querySelector<HTMLElement>('.ticket-panel') ?? node?.querySelector<HTMLElement>('#card-detail-title'))?.focus();
    return () => { node?.close(); };
  }, [showing]);
  const panels: Record<PanelView, ReactNode> = {
    board: <TicketBoard floor={floor} selectedId={selectedCardId} onSelect={openCard}
      forming={overlay?.kind === 'new'} onNewTicket={openNewTicket} />,
    log: <LogPanel floor={floor} />,
    setup: <SetupView floor={floor} onGuide={onGuide} />,
    settings: <SettingsView floor={floor} onSetup={() => onView('setup')} />,
  };
  const label = labelOf(view);
  const page = isPageView(view);
  return (
    <section id="control-panel" data-view={view} aria-labelledby={CONTROL_TITLE_ID}
      className={cn('control-panel relative z-(--layer-panels) box-border flex min-h-0 min-w-0 flex-col overflow-y-auto border-0 bg-bg font-sans text-ink',
        page ? 'static col-[1/-1] row-1 gap-4 px-[max(16px,calc((100%-832px)/2))] py-6'
          : 'col-[1/-1] row-2 gap-6 px-9 pb-8 pt-6 max-[800px]:p-5')}
      data-page={page ? 'true' : undefined}
      ref={handFocusOnLeave} data-focus-on-leave={viewButtonId(view)}
      onKeyDown={(event) => { if (closesPanel(event)) { event.stopPropagation(); onClose(); } }}>
      <header className="control-panel-head flex flex-none items-center justify-between gap-2">
        <div className="control-heading-copy min-w-0">
          <h2 id={CONTROL_TITLE_ID} tabIndex={-1} ref={focusOnArrival}
            className="m-0 text-page font-semibold tracking-tight text-ink focus:outline-0">{label}</h2>
          {view === 'board' ? <p className="board-description m-0 mt-1 text-small text-muted">Give work a clear next step.</p> : null}
        </div>
        {page ? null : <IconButton label={`Close ${label}`} title="Close (Escape)" className="panel-close" icon={<X {...ICON} />}
          onClick={onClose} />}
      </header>
      <div className={cn('control-view min-h-0 min-w-0 overflow-auto', page || view === 'board' ? 'flex-none overflow-visible' : 'flex-auto')}>{panels[view]}</div>
      {view === 'board' ? <TicketNotice floor={floor}
        card={showing ? overlay?.kind === 'card' ? overlay.id : null : undefined} /> : null}
      {view === 'board' ? <dialog className="ticket-dialog" id="selected-card-detail" ref={dialog}
        data-composer={overlay?.kind === 'new'}
        aria-label={overlay?.kind === 'new' ? 'New ticket' : 'Ticket detail'}
        onCancel={(event) => { event.preventDefault(); closeOverlay(); }}
        onKeyDown={(event) => { if (event.key === 'Escape') event.stopPropagation(); }}>
        {showing ? <><header className="ticket-dialog-head"><p>{overlay?.kind === 'new' ? 'New ticket' : 'Ticket review'}</p>
          <button type="button" className="ticket-dialog-close" aria-label="Close ticket dialog" onClick={closeOverlay}>×</button>
        </header>
        {overlay?.kind === 'new' ? <div className="ticket-composer"><h3>New ticket</h3>
          <p>Tell an agent what you want to happen.</p>
          {floor.ticket ? null : <TicketOffHint floor={floor} />}
          <NewTicketForm floor={floor} onHire={onHire} onCancel={closeOverlay}
            onCreated={(id) => { opener.current = null; seenCard.current = null; setOverlay({ kind: 'card', id }); }} />
        </div> : selectedCard?.ticket != null ? <TicketPanel key={selectedCard.id} floor={floor} card={selectedCard} onHire={onHire}
          onReviewRecovery={onReviewRecovery} />
          : selectedCard !== null ? <CardDetail key={selectedCard.id} floor={floor} card={selectedCard} />
            : <p role="status">Waiting for the board to confirm this ticket.</p>}
        </> : null}
      </dialog> : null}
    </section>
  );
}
