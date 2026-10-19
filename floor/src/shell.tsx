import { useEffect, useRef, useState, useSyncExternalStore } from 'react';
import type { CSSProperties, KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from 'react';

import { firstLinkProblem, pastedLinkBuildMismatch, pastedLinkRefused, subscribeToPastedLink } from './connect';
import { AgentDrawer, Receipts } from './renderer/AgentDrawer';
import { CONTROL_VIEWS, Control, ControlNav, FocusRoster, toggledView, viewButtonId } from './renderer/Control';
import type { ControlView } from './renderer/Control';
import { CrewList, drawerHandBack } from './renderer/CrewList';
import { CrewStats } from './renderer/CrewTotals.tsx';
import { PauseAllControl } from './renderer/control/PauseAllControl';
import { DoneNotices } from './renderer/DoneNotices';
import { NeedsStrip } from './renderer/NeedsStrip';
import { OfficeCanvas } from './renderer/OfficeCanvas';
import { getFloorState, selectOutputAgent, subscribeToFloor } from './renderer/floor/store';
import { refreshFloorSnapshot } from './renderer/floor/stream';
import { HireWizard } from './renderer/onboarding/HireWizard';
import { HireEntry, hireSwitchOn } from './renderer/onboarding/HireEntry';
import { FirstRunGuide } from './renderer/setup/FirstRunGuide';
import { FirstTask } from './renderer/setup/FirstTask.tsx';
import {
  agentToReveal, getHireState, markRevealed, prepareArchiveHire, prepareHire, subscribeToHire,
} from './renderer/onboarding/hire';
import type { RetiredAgentRow } from './renderer/archive';
import { AGENT_DETAIL_HEADING_ID, closesDrawer, focusAnswer, handFocusOnLeave, restoreFocusAfterDialog } from './renderer/focus';
import { RetireDialog } from './renderer/retire/RetirePanel';
import { selectedAgentRow } from './renderer/scene/selection';
import { TerminalPanel } from './renderer/terminal/TerminalPanel';
import { JournalStoppedBanner } from './renderer/JournalStoppedBanner';
import { ClosingTimeBanner } from './renderer/ClosingTimeBanner';
import { TicketBanner } from './renderer/ticket/TicketBanner';
import { useDesktopNotifications } from './renderer/useDesktopNotifications';
import { requestCardSelection } from './renderer/BoardPanel';
import { SignInExpired } from './renderer/SignInExpired';
import { StreamNotice } from './renderer/StreamNotice';
import './failure.css';
import {
  DEFAULT_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH, MIN_SIDEBAR_WIDTH,
  focusAgentId, isFocusShortcut, keyboardSidebarWidth, pointerSidebarWidth,
  readViewPreferences, rememberGuideClosed, toggleTheme, writeViewPreferences,
} from './renderer/view';

/**
 * Said when the link this page was opened with, or one pasted into it, was
 * refused. Without it the page looks as it did, which reads as the link having
 * worked. It names no part of the link: the secret stays out of the page. The
 * daemon does not say why it refused a first link, so the page names both
 * causes, a link that was already used or expired and a full set of four Floor
 * sessions, with a way out of each. A closed tab frees its place only once its
 * sign-in lapses after 30 minutes without use; stopping and starting the daemon
 * ends every sign-in at once. A page opened with no link, as a
 * copied address is because a browser
 * scrubs the secret as soon as it signs in, is told its sign-in has expired.
 */
export function PastedLinkNotice() {
  const first = useSyncExternalStore(subscribeToPastedLink, firstLinkProblem, firstLinkProblem);
  const refused = useSyncExternalStore(subscribeToPastedLink, pastedLinkRefused, pastedLinkRefused);
  const buildMismatch = useSyncExternalStore(subscribeToPastedLink, pastedLinkBuildMismatch, pastedLinkBuildMismatch);
  if (first === 'refused') {
    return (
      <p className="link-refused" role="alert">
        This sign-in link was refused, either because it was already used or has expired, or because four Floor tabs are
        already signed in. For a used or expired link, run <code>dudes floor open</code> for a new one and paste it here
        within 60 seconds. For a full set of tabs, close the ones you no longer use and wait 30 minutes, or run{' '}
        <code>dudes stop</code> then <code>dudes start</code> to sign every tab out (this ends running turns), and then{' '}
        <code>dudes floor open</code>.
      </p>
    );
  }
  if (first === 'missing') {
    return (
      <p className="link-refused" role="alert">
        The sign-in link in the address of this page is not one the daemon can read. Run{' '}
        <code>dudes floor open</code> and paste the new link here within 60 seconds.
      </p>
    );
  }
  if (first === 'expired') return <SignInExpired />;
  if (!refused) return null;
  if (buildMismatch) {
    return (
      <p className="link-refused" role="alert">
        The daemon now serves a different Floor build from this page, so the link pasted into this tab was not
        used. This page is as it was, but it cannot sign in to the new build. Copy any draft you need, reload
        this tab, then paste the link again.
      </p>
    );
  }
  return (
    <p className="link-refused" role="alert">
      The Floor link pasted into this tab was refused, or the daemon could not be reached to take it. This page is
      as it was and keeps what it holds. For a new link, run dudes floor open and paste it into this
      tab’s address bar.
    </p>
  );
}

/**
 * What a page with no session shows: the reason and its remedy, and nothing
 * that reads as a live office. The crew, hire, checks and board panels would
 * all be empty for want of a session, which looks like a daemon that is down.
 */
function SignedOutShell() {
  return (
    <main className="shell shell-signed-out" aria-labelledby="floor-title" data-theme={readViewPreferences().theme}>
      <header className="top-bar">
        <h1 id="floor-title" className="wordmark">dudes</h1>
        <PastedLinkNotice />
      </header>
      <p className="signed-out-line" role="status">This tab is not signed in, so the Floor shows no agents yet.</p>
    </main>
  );
}

export function WebFloorShell() {
  const firstLink = useSyncExternalStore(subscribeToPastedLink, firstLinkProblem, firstLinkProblem);
  return firstLink === null ? <SignedInShell /> : <SignedOutShell />;
}

function SignedInShell() {
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [hiring, setHiring] = useState(false);
  const [guide, setGuide] = useState<'open' | 'closed'>('closed');
  const [archiveHireKept, setArchiveHireKept] = useState(false);
  const [officeBar, setOfficeBar] = useState<HTMLDivElement | null>(null);
  const hire = useSyncExternalStore(subscribeToHire, getHireState, getHireState);
  const [view, setView] = useState<ControlView | null>(null);
  const [answerAsked, setAnswerAsked] = useState<{ readonly id: string } | null>(null);
  const [crewOpen, setCrewOpen] = useState(true);
  const [focused, setFocused] = useState(false);
  const [preferences, setPreferences] = useState(readViewPreferences);
  const [osDark, setOsDark] = useState(() => typeof window !== 'undefined'
    && window.matchMedia('(prefers-color-scheme: dark)').matches);
  const drag = useRef<{ pointerId: number; x: number; width: number } | null>(null);
  const hireDialog = useRef<HTMLDialogElement | null>(null);
  const hireOpener = useRef<HTMLElement | null>(null);
  const backdropPress = useRef<number | null>(null);
  const selectedRow = selectedAgentRow(floor.agents, selectedId, floor.phase, floor.stale);
  const focusedId = focused ? focusAgentId(floor, selectedId) : null;
  const displayRow = focused ? selectedAgentRow(floor.agents, focusedId, floor.phase, floor.stale) : selectedRow;
  const currentSelectedId = displayRow?.id ?? null;
  const selectable = floor.phase === 'live' && !floor.stale;
  const effectiveTheme = preferences.theme === 'system' ? (osDark ? 'dark' : 'light') : preferences.theme;
  const reveal = agentToReveal(hire, floor);
  const drawerOpen = displayRow !== null || floor.output.targetId !== null;
  const home = focused ? 'focus-toggle' : 'crew-toggle';
  const leaveDrawerTo = drawerHandBack({ agent: currentSelectedId, focused, crewOpen, viewOpen: view !== null });

  useDesktopNotifications((target) => {
    setGuide('closed');
    if (hiring) closeHire(true);
    if (target.kind === 'ticket') openCard(target.id);
    else { setFocused(false); setView(null); setCrewOpen(true); openQuestion(target.id); }
  });

  useEffect(() => {
    if (reveal === null) return;
    markRevealed(reveal);
    selectAgent(reveal);
    if (hire.outcome?.kind === 'complete') {
      closeHire(true);
      setGuide('closed');
      setFocused(false);
      setArchiveHireKept(false);
    }
  }, [reveal, hire.outcome?.kind]);

  useEffect(() => {
    writeViewPreferences(preferences);
  }, [preferences]);

  useEffect(() => {
    const preference = window.matchMedia('(prefers-color-scheme: dark)');
    const onThemePreference = () => setOsDark(preference.matches);
    preference.addEventListener('change', onThemePreference);
    return () => preference.removeEventListener('change', onThemePreference);
  }, []);

  useEffect(() => {
    if (selectedId !== null && selectable && floor.lsn > 0 && selectedRow === null) {
      selectOutputAgent(null);
      setSelectedId(null);
    }
  }, [selectedId, selectedRow, selectable, floor.lsn]);

  useEffect(() => {
    if (answerAsked === null || currentSelectedId !== answerAsked.id) return;
    setAnswerAsked(null);
    focusAnswer(document.getElementById('selected-agent-detail'));
  }, [answerAsked, currentSelectedId]);

  useEffect(() => {
    if (focused && selectedId === null && focusedId !== null) {
      selectOutputAgent(focusedId);
      setSelectedId(focusedId);
    }
  }, [focused, selectedId, focusedId]);

  useEffect(() => {
    const onShortcut = (event: KeyboardEvent) => {
      const target = event.target;
      const blocked = target instanceof Element && target.closest(
        '.terminal-panel, .hire-wizard, .ticket-dialog, .first-run-guide, input, textarea, select',
      ) !== null;
      if (!isFocusShortcut(event, blocked)) return;
      event.preventDefault();
      toggleFocus();
    };
    document.addEventListener('keydown', onShortcut);
    return () => document.removeEventListener('keydown', onShortcut);
  }, [focused, floor, selectedId]);

  function selectAgent(id: string): boolean {
    if (!selectable || !floor.agents.some((row) => row.id === id)) return false;
    selectOutputAgent(id);
    setSelectedId(id);
    return true;
  }

  function openQuestion(id: string): void {
    if (selectAgent(id)) setAnswerAsked({ id });
  }

  function openView(next: ControlView): void {
    setFocused(false);
    setView(next);
    if (CONTROL_VIEWS.some((entry) => entry.view === next && entry.refresh)) refreshFloorSnapshot();
  }

  function toggleView(next: ControlView): void {
    const target = toggledView(view, next, focused);
    if (target === null) setView(null);
    else openView(target);
  }

  function toggleCrew(): void {
    if (view === 'board') {
      setView(null);
      setFocused(false);
      setCrewOpen(true);
    } else if (focused) {
      setFocused(false);
      setCrewOpen(true);
    } else {
      setCrewOpen((open) => !open);
    }
  }

  function openCard(id: string): void {
    if (!selectable || !floor.cards.some((card) => card.id === id)) return;
    requestCardSelection(id);
    openView('board');
  }

  function hireAgain(row: RetiredAgentRow, opener: HTMLButtonElement): void {
    backdropPress.current = null;
    hireOpener.current = opener;
    setArchiveHireKept(!prepareArchiveHire(row, floor));
    setGuide('closed');
    setHiring(true);
  }

  function openHire(opener: HTMLElement | null): void {
    backdropPress.current = null;
    hireOpener.current = opener;
    prepareHire(floor);
    setHiring(true);
  }

  function closeHire(revealed = false): void {
    backdropPress.current = null;
    hireDialog.current?.close();
    setHiring(false);
    const ticketDialog = document.getElementById('selected-card-detail');
    const ticketFallback = ticketDialog?.getAttribute('open') != null
      ? ticketDialog.querySelector<HTMLElement>('.ticket-dialog-close') : null;
    if (!revealed || ticketFallback !== null) {
      restoreFocusAfterDialog(hireOpener.current,
        ticketFallback ?? document.getElementById('hire-open') ?? document.getElementById('crew-toggle'));
    }
  }

  useEffect(() => {
    if (!hiring) return;
    const dialog = hireDialog.current;
    dialog?.showModal();
    dialog?.querySelector<HTMLElement>('#hire-title')?.focus();
  }, [hiring]);

  function clearSelection(): void {
    if (focused) setFocused(false);
    selectOutputAgent(null);
    setSelectedId(null);
  }

  function toggleFocus(): void {
    if (!focused) {
      setHiring(false);
      const id = focusAgentId(floor, selectedId);
      if (id !== null) selectAgent(id);
    }
    setFocused((active) => !active);
    document.getElementById('focus-toggle')?.focus();
  }

  function startResize(event: ReactPointerEvent<HTMLDivElement>): void {
    if (!event.isPrimary || event.button !== 0) return;
    drag.current = { pointerId: event.pointerId, x: event.clientX, width: preferences.sidebarWidth };
    event.currentTarget.setPointerCapture(event.pointerId);
    event.preventDefault();
  }

  function moveResize(event: ReactPointerEvent<HTMLDivElement>): void {
    const start = drag.current;
    if (start === null || event.pointerId !== start.pointerId) return;
    const width = pointerSidebarWidth(start.width, start.x, event.clientX);
    setPreferences((current) => ({ ...current, sidebarWidth: width }));
  }

  function endResize(event: ReactPointerEvent<HTMLDivElement>): void {
    const start = drag.current;
    if (start === null || event.pointerId !== start.pointerId) return;
    const width = pointerSidebarWidth(start.width, start.x, event.clientX);
    setPreferences((current) => ({ ...current, sidebarWidth: width }));
    drag.current = null;
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
  }

  function keyResize(event: ReactKeyboardEvent<HTMLDivElement>): void {
    const width = keyboardSidebarWidth(preferences.sidebarWidth, event.key);
    if (width === null) return;
    event.preventDefault();
    setPreferences((current) => ({ ...current, sidebarWidth: width }));
  }

  function resetSidebar(): void {
    setPreferences((current) => ({ ...current, sidebarWidth: DEFAULT_SIDEBAR_WIDTH }));
  }

  const splitter = <div className="panel-splitter" role="separator" tabIndex={0}
    aria-label={focused ? 'Resize roster' : 'Resize agent details'}
    aria-orientation="vertical" aria-valuemin={MIN_SIDEBAR_WIDTH} aria-valuemax={MAX_SIDEBAR_WIDTH}
    aria-valuenow={preferences.sidebarWidth} aria-valuetext={`${preferences.sidebarWidth} pixels`}
    onPointerDown={startResize} onPointerMove={moveResize} onPointerUp={endResize}
    onPointerCancel={endResize} onKeyDown={keyResize} onDoubleClick={resetSidebar} />;
  const drawer = <AgentDrawer floor={floor} row={displayRow} selectedId={currentSelectedId} onClose={clearSelection}
    leaveFocusTo={leaveDrawerTo} />;

  return (
    <main className="shell" aria-labelledby="floor-title" data-focus={focused ? 'true' : 'false'}
      data-theme={preferences.theme} style={{ '--side-width': `${preferences.sidebarWidth}px` } as CSSProperties}>
      <header className="top-bar">
        <h1 id="floor-title" className="wordmark">dudes</h1>
        <CrewStats agents={floor.agents} cards={floor.cards} />
        <ControlNav floor={floor} view={focused ? null : view} onToggle={toggleView} />
        <div className="top-bar-actions" role="group" aria-label="Floor actions">
          <button type="button" className="crew-toggle" id="crew-toggle" aria-pressed={crewOpen && !focused && view !== 'board'}
            aria-controls={crewOpen && !focused && view !== 'board' ? 'crew-list' : undefined}
            onClick={toggleCrew}>Crew</button>
          <button type="button" className="focus-toggle" id="focus-toggle" aria-pressed={focused}
            aria-keyshortcuts="Alt+Shift+F" onClick={toggleFocus}>
            {focused ? 'Leave focus' : 'Focus agent'}
          </button>
          {hireSwitchOn(floor) ? <button type="button" className="hire-open" id="hire-open" aria-expanded={hiring}
            aria-controls={hiring ? 'hire-wizard' : undefined}
            onClick={(event) => openHire(event.currentTarget)}>Hire an agent</button> : null}
          <PauseAllControl floor={floor} />
          <button type="button" className="theme-toggle" aria-pressed={effectiveTheme === 'dark'}
            onClick={() => setPreferences((current) => ({ ...current, theme: toggleTheme(current.theme, osDark) }))}>
            Use {effectiveTheme === 'dark' ? 'light' : 'dark'} theme
          </button>
        </div>
        <PastedLinkNotice />
        <JournalStoppedBanner stopped={floor.journalStopped} />
        <ClosingTimeBanner closing={floor.closingDisplay?.phase ?? null} progress={floor.closingDisplay?.progress ?? null} stale={floor.stale || floor.closingTime === null} />
        {focused || floor.agents.length === 0 ? <TicketBanner floor={floor} onOpen={() => openView('board')} /> : null}
        <StreamNotice floor={floor} />
        <div className="office-bar" ref={setOfficeBar} />
      </header>
      <div className="floor-stage"
        onKeyDown={(event) => { if (closesDrawer(event, drawerOpen)) { event.stopPropagation(); clearSelection(); } }}>
        {crewOpen && !focused ? <CrewList agents={floor.agents} cards={floor.cards} selectedId={currentSelectedId}
          selectable={selectable} stale={floor.stale} onSelectAgent={selectAgent}
          onOpenCard={openCard}
          addAgent={<HireEntry floor={floor} id="hire-open-crew" onAddAgent={openHire} folded />} /> : null}
        {view === null || focused ? null : <Control key={view} floor={floor} view={view} selectedId={currentSelectedId}
          onClose={() => setView(null)} onView={openView} onOpenQuestion={openQuestion} onOpenTicket={openCard}
          onHire={openHire} onHireAgain={hireAgain}
          onGuide={() => { setFocused(false); setHiring(false); setGuide('open'); }} />}
        <div className="office-slot">
          <div className="office-area">
            <OfficeCanvas floor={floor} selectedId={currentSelectedId} onSelectAgent={selectAgent} onOpenQuestion={openQuestion}
              onOpenTasks={() => openView('board')} onAddAgent={openHire} bar={officeBar} />
            <FirstTask floor={floor} onHire={openHire} />
          </div>
          {focused ? null : <NeedsStrip agents={floor.agents} cards={floor.cards} selectedId={currentSelectedId}
            selectable={selectable} stale={floor.stale} onSelectAgent={selectAgent} onOpenQuestion={openQuestion}
            onOpenTicket={openCard} />}
        </div>
        <DoneNotices notices={floor.doneNotices} leaveFocusTo={home} />
        <div className="stage-center">
          <TerminalPanel floor={floor} theme={effectiveTheme} />
        </div>
        <div className="stage-side">
          <Receipts floor={floor} leaveFocusTo={displayRow === null ? home : AGENT_DETAIL_HEADING_ID} />
          {focused ? <section className="focus-mode" aria-label="Focus mode" ref={handFocusOnLeave}
            data-focus-on-leave="focus-toggle">
            <h2>Focused agent</h2>
            <div className="focus-frame">
              {drawer}
              {splitter}
              <FocusRoster floor={floor} selectedId={currentSelectedId} onSelectAgent={selectAgent}
                onHireAgain={hireAgain} onAddAgent={openHire} collapsed={preferences.rosterCollapsed}
                onToggle={() => setPreferences((current) => ({ ...current, rosterCollapsed: !current.rosterCollapsed }))} />
            </div>
          </section> : drawerOpen ? <div className="drawer-frame">{splitter}{drawer}</div> : null}
        </div>
        {guide === 'open' ?
          <FirstRunGuide floor={floor} onClose={() => {
            rememberGuideClosed(); setGuide('closed');
            (document.getElementById('first-run-guide-open') ?? document.getElementById(viewButtonId('setup'))
              ?? document.getElementById('hire-open'))?.focus();
          }} onHire={(opener) => openHire(opener)} /> : null}
      </div>
      <RetireDialog floor={floor} />
      {hiring ? <dialog className="hire-dialog" ref={hireDialog}
        aria-labelledby="hire-title" onCancel={(event) => { event.preventDefault(); closeHire(); }}
        onPointerDown={(event) => {
          backdropPress.current = event.target === event.currentTarget && event.button === 0 && event.isPrimary
            ? event.pointerId : null;
        }}
        onPointerUp={(event) => {
          const dismiss = backdropPress.current === event.pointerId
            && event.currentTarget.ownerDocument.elementFromPoint(event.clientX, event.clientY) === event.currentTarget
            && event.button === 0 && event.isPrimary;
          backdropPress.current = null;
          if (dismiss) closeHire();
        }}
        onPointerCancel={() => { backdropPress.current = null; }}>
        {archiveHireKept ? <p role="status">The current Hire is kept. Resolve it before preparing another agent.</p> : null}
        <HireWizard floor={floor} onClose={() => closeHire()} />
      </dialog> : null}
    </main>
  );
}
