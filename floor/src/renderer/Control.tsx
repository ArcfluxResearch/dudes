// SPDX-License-Identifier: MIT OR Apache-2.0

import { Maximize2, X } from 'lucide-react';
import type { ReactNode } from 'react';
import { useEffect, useLayoutEffect, useRef, useState } from 'react';

import { ArchivedAgents } from './ArchivedAgents.tsx';
import type { RetiredAgentRow } from './archive.ts';
import { LogPanel } from './LogPanel.tsx';
import { Roster } from './Roster.tsx';
import { closesPanel, handFocusOnLeave, restoreFocusAfterDialog } from './focus.ts';
import { StreamReadStatus } from './ReadError.tsx';
import type { FloorState } from './floor/store.ts';
import { ProjectsView } from './project/ProjectsView.tsx';
import { SetupView } from './setup/SetupView.tsx';
import { SettingsView } from './settings/SettingsView.tsx';
import { CardDetail } from './BoardPanel.tsx';
import { NewTicketForm, TicketBoard, TicketNotice, ticketActionsOpen } from './ticket/TicketBoard.tsx';
import { CopyLink } from './ticket/CopyLink.tsx';
import { showProject } from './ticket/browseState.ts';
import { useRequestedNewTicket } from './ticket/newTicketRequest.ts';
import { TicketPage } from './ticket/TicketPage.tsx';
import { readNewTicketDraft, writeNewTicketDraft } from './view.ts';
import { viewConfirmed } from './ticket/model.ts';
import { TicketOffHint } from './ticket/TicketGuidance.tsx';
import { TicketPanel } from './ticket/TicketPanel.tsx';
import { HireEntry } from './onboarding/HireEntry.tsx';
import { isPageView } from '../navigation.ts';
import { Button, Dialog, ICON, IconButton, cn } from '../ui/index.ts';

export type ControlView = 'overview' | 'board' | 'projects' | 'log' | 'setup' | 'settings';

export type PanelView = Exclude<ControlView, 'overview'>;

/** Control's views, in top bar order. Projects, Setup and Settings take a fresh snapshot when they open. */
export const CONTROL_VIEWS: readonly { readonly view: ControlView; readonly label: string; readonly refresh: boolean }[] = [
  { view: 'overview', label: 'Overview', refresh: false },
  { view: 'board', label: 'Board', refresh: false },
  { view: 'projects', label: 'Projects', refresh: true },
  { view: 'log', label: 'Log', refresh: false },
  { view: 'setup', label: 'Setup', refresh: true },
  { view: 'settings', label: 'Settings', refresh: true },
];

export const CONTROL_TITLE_ID = 'control-title';

type BoardOverlay = { readonly kind: 'new' } | { readonly kind: 'card'; readonly id: string; readonly full: boolean } | null;

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
    <div className="agents-roster h-full overflow-auto">
      <StreamReadStatus floor={floor} className="m-0 mb-[0.45rem] font-[700]" />
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
    <section className="focus-roster grid min-h-0 content-start gap-3 overflow-auto max-[801px]:order-first max-[801px]:max-h-56" aria-label="Focus roster">
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
    <nav className="control-nav flex flex-wrap items-center gap-1 max-[801px]:order-3 max-[801px]:basis-full max-[801px]:justify-between" aria-label="Views">
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
  const open = view === 'board' && overlay !== null;
  const full = open && overlay?.kind === 'card' && overlay.full;
  const showing = open && !full;
  const scrolled = useRef(0);
  const wasFull = useRef(false);

  function openCard(id: string, from?: HTMLElement, asPage = false): void {
    opener.current = from ?? document.querySelector<HTMLButtonElement>(`[data-card-id="${id}"]`);
    seenCard.current = null;
    setOverlay({ kind: 'card', id, full: asPage });
  }

  function openNewTicket(from: HTMLElement | null): void {
    opener.current = from;
    seenCard.current = null;
    setOverlay({ kind: 'new' });
  }

  function expandCard(): void {
    if (overlay?.kind !== 'card') return;
    scrolled.current = document.getElementById('control-panel')?.scrollTop ?? 0;
    setOverlay({ kind: 'card', id: overlay.id, full: true });
  }

  useRequestedNewTicket(view === 'board' && ticketActionsOpen(floor), (title) => {
    if (title !== '') writeNewTicketDraft({ ...readNewTicketDraft(), title });
    openNewTicket(document.activeElement instanceof HTMLElement ? document.activeElement : null);
  });

  useLayoutEffect(() => {
    const panel = document.getElementById('control-panel');
    if (panel !== null && full !== wasFull.current) panel.scrollTop = full ? 0 : scrolled.current;
    wasFull.current = full;
  }, [full]);

  useEffect(() => {
    if (full) document.querySelector<HTMLElement>('.ticket-page .ticket-panel, .ticket-page #card-detail-title')?.focus();
  }, [full]);

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
  const ticketBody = selectedCard?.ticket != null ? <TicketPanel key={selectedCard.id} floor={floor} card={selectedCard} onHire={onHire}
    onReviewRecovery={onReviewRecovery} />
    : selectedCard !== null ? <CardDetail key={selectedCard.id} floor={floor} card={selectedCard} />
      : <p role="status" className="m-0 p-6 text-base text-muted">Waiting for the board to confirm this ticket.</p>;
  const panels: Record<PanelView, ReactNode> = {
    board: <>
      <div className={cn(full && 'hidden')}><TicketBoard floor={floor} selectedId={selectedCardId} onSelect={openCard}
        forming={overlay?.kind === 'new'} onNewTicket={openNewTicket} paused={full} /></div>
      {full && selectedCardId !== null ? <TicketPage id={selectedCardId} onBack={closeOverlay}>{ticketBody}</TicketPage> : null}
    </>,
    projects: <ProjectsView floor={floor} onOpenBoard={(project) => { showProject(project); onView('board'); }} />,
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
          : 'col-[1/-1] row-2 gap-6 px-9 pb-8 pt-6 max-[801px]:m-2 max-[801px]:max-h-128 max-[801px]:rounded-[10px] max-[801px]:p-5', view === 'board' && 'has-[dialog[open]]:overflow-y-hidden')}
      data-page={page ? 'true' : undefined}
      ref={handFocusOnLeave} data-focus-on-leave={viewButtonId(view)}
      onKeyDown={(event) => { if (closesPanel(event)) { event.stopPropagation(); if (full) closeOverlay(); else onClose(); } }}>
      <header className="control-panel-head flex flex-none items-center justify-between gap-2">
        <div className="control-heading-copy min-w-0">
          <h2 id={CONTROL_TITLE_ID} tabIndex={-1} ref={focusOnArrival}
            className="m-0 text-page font-semibold tracking-[-0.01em] text-ink focus:outline-0">{label}</h2>
          {view === 'board' ? <p className="board-description m-0 mt-1 text-small text-muted">Give work a clear next step.</p> : null}
        </div>
        {page ? null : <IconButton label={`Close ${label}`} title="Close (Escape)" className="panel-close" icon={<X {...ICON} />}
          onClick={onClose} />}
      </header>
      <div className={cn('control-view min-h-0 min-w-0 overflow-auto', page || view === 'board' ? 'flex-none overflow-visible' : 'flex-auto')}>{panels[view]}</div>
      {view === 'board' ? <TicketNotice floor={floor}
        card={open ? overlay?.kind === 'card' ? overlay.id : null : undefined} /> : null}
      {view === 'board' ? <Dialog className="ticket-dialog focus-visible:outline-2 focus-visible:outline-solid focus-visible:-outline-offset-2 focus-visible:outline-focus m-0 ml-auto h-dvh max-h-none w-[min(100vw,56rem,max(35rem,70vw))] max-w-[100vw] rounded-none rounded-l-xl border-y-0 border-r-0 data-[composer=true]:m-auto data-[composer=true]:h-fit data-[composer=true]:max-h-[calc(100dvh-4rem)] data-[composer=true]:w-[min(36rem,calc(100vw-2rem))] data-[composer=true]:rounded-xl data-[composer=true]:border max-[801px]:data-[composer=false]:w-screen max-[801px]:data-[composer=false]:rounded-none"
        id="selected-card-detail" ref={dialog}
        data-composer={overlay?.kind === 'new'}
        aria-label={overlay?.kind === 'new' ? 'New ticket' : 'Ticket detail'}
        onCancel={(event) => { event.preventDefault(); closeOverlay(); }}
        onKeyDown={(event) => { if (event.key === 'Escape') event.stopPropagation(); }}>
        {showing ? <><header className={cn('ticket-dialog-head sticky top-0 z-10 flex flex-wrap items-center justify-between gap-2 border-b border-line bg-surface px-6 py-2',
          overlay?.kind === 'new' && 'border-b-0 pb-0')}>
          <p className={cn('m-0 text-caption font-semibold tracking-label text-muted uppercase', overlay?.kind === 'new' && 'sr-only')}>{overlay?.kind === 'new' ? 'New ticket' : 'Ticket review'}</p>
          <div className="ml-auto flex items-center gap-2">
            {overlay?.kind === 'card' ? <>
              <CopyLink id={overlay.id} />
              <Button size="sm" icon={<Maximize2 {...ICON} />} className="ticket-expand" onClick={expandCard}>Full page</Button>
            </> : null}
            <IconButton label="Close ticket dialog" icon={<X {...ICON} />} className="ticket-dialog-close" onClick={closeOverlay} />
          </div>
        </header>
        {overlay?.kind === 'new' ? <div className="ticket-composer flex flex-col gap-2 px-6 pt-2 pb-6"><h3 className="m-0 text-page font-semibold">New ticket</h3>
          <p className="m-0 text-base text-muted">Tell an agent what you want to happen.</p>
          {floor.ticket ? null : <TicketOffHint floor={floor} />}
          <NewTicketForm floor={floor} onHire={onHire} onCancel={closeOverlay}
            onCreated={(id) => { opener.current = null; seenCard.current = null; setOverlay({ kind: 'card', id, full: false }); }} />
        </div> : ticketBody}
        </> : null}
      </Dialog> : null}
    </section>
  );
}
