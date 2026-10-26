import { useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore } from 'react';
import type { CSSProperties, KeyboardEvent as ReactKeyboardEvent, PointerEvent as ReactPointerEvent } from 'react';
import { createPortal } from 'react-dom';
import { currentPage, isPageView, openPage, subscribeToPage } from './navigation.ts';

import { DeviceNotices } from './renderer/devices/DeviceNotices';
import { getDevicesState, loadDevices, subscribeToDevices } from './renderer/devices/devices';
import { signOutHere } from './renderer/devices/signOut';
import {
  firstLinkProblem, pastedLinkBuildMismatch, pastedLinkRefused, pastedLinkWaitsForReload, subscribeToPastedLink,
} from './connect';
import { AgentDrawer, Receipts } from './renderer/AgentDrawer';
import { CONTROL_TITLE_ID, CONTROL_VIEWS, Control, ControlNav, FocusRoster, toggledView, viewButtonId } from './renderer/Control';
import type { ControlView } from './renderer/Control';
import { drawerHandBack, Roster } from './renderer/Roster';
import { CrewStats } from './renderer/CrewTotals.tsx';
import { PauseAllControl } from './renderer/control/PauseAllControl';
import { DoneNotices } from './renderer/DoneNotices';
import { OverviewDialog } from './renderer/OverviewDialog';
import { OfficeCanvas } from './renderer/OfficeCanvas';
import { getFloorState, selectOutputAgent, subscribeToFloor } from './renderer/floor/store';
import { refreshFloorSnapshot } from './renderer/floor/stream';
import { Dialog, Notice } from './ui/index.ts';
import { HireWizard } from './renderer/onboarding/HireWizard';
import { HireEntry, hireSwitchOn } from './renderer/onboarding/HireEntry';
import { FirstRunGuide } from './renderer/setup/FirstRunGuide';
import { NotificationOffer } from './renderer/NotificationOffer.tsx';
import { FirstTask } from './renderer/setup/FirstTask.tsx';
import {
  agentToReveal, getHireState, leaveHire, ticketsOff, markRevealed, prepareArchiveHire, prepareHire, subscribeToHire,
} from './renderer/onboarding/hire';
import type { RetiredAgentRow } from './renderer/archive';
import { AGENT_DETAIL_HEADING_ID, closesDrawer, closesPanel, focusAnswer, handFocusOnLeave, restoreFocusAfterDialog } from './renderer/focus';
import { RetireDialog } from './renderer/retire/RetirePanel';
import { selectedAgentRow } from './renderer/scene/selection';
import { TerminalPanel } from './renderer/terminal/TerminalPanel';
import { JournalStoppedBanner } from './renderer/JournalStoppedBanner';
import { ClosingTimeBanner } from './renderer/ClosingTimeBanner';
import { TicketBanner } from './renderer/ticket/TicketBanner';
import { TicketNotice, TicketNoticeHost } from './renderer/ticket/TicketBoard.tsx';
import { useDesktopNotifications } from './renderer/useDesktopNotifications';
import { requestCardSelection } from './renderer/BoardPanel';
import { OpenCommand, SignInExpired } from './renderer/SignInExpired';
import { StreamNotice } from './renderer/StreamNotice';
import { NoticeStack } from './renderer/notices/NoticeStack';
import './failure.css';
import {
  DEFAULT_SIDEBAR_WIDTH, MAX_SIDEBAR_WIDTH, MIN_SIDEBAR_WIDTH,
  focusAgentId, isFocusShortcut, keyboardSidebarWidth, pointerSidebarWidth,
  readViewPreferences, rememberGuideClosed, toggleTheme, writeViewPreferences,
} from './renderer/view';

function reloadPage(): void {
  window.location.reload();
}

/**
 * Said when the link this page was opened with, or one pasted into it, was
 * refused. Without it the page looks as it did, which reads as the link having
 * worked. It names no part of the link: the secret stays out of the page. A
 * link is refused only when it was already used or has expired, since a full
 * set of four Floor sessions makes room by ending the least recently used one.
 * A page opened with no link, as a copied address is because a browser scrubs
 * the secret as soon as it signs in, is told it is signed out.
 */
export function PastedLinkNotice() {
  const first = useSyncExternalStore(subscribeToPastedLink, firstLinkProblem, firstLinkProblem);
  const refused = useSyncExternalStore(subscribeToPastedLink, pastedLinkRefused, pastedLinkRefused);
  const buildMismatch = useSyncExternalStore(subscribeToPastedLink, pastedLinkBuildMismatch, pastedLinkBuildMismatch);
  const waiting = useSyncExternalStore(subscribeToPastedLink, pastedLinkWaitsForReload, pastedLinkWaitsForReload);
  if (waiting) {
    return (
      <p className="link-refused" role="alert">
        A new sign-in link was opened in this tab. Reload this page to use the new link. A reload drops anything
        you have typed but not sent. <button type="button" className="notice-action"
          onClick={reloadPage}>Reload</button>
      </p>
    );
  }
  if (first === 'refused') {
    return (
      <p className="link-refused" role="alert">
        This sign-in link did not work. A link works once, for 60 seconds. To get a new one, run <OpenCommand /> in a terminal.
      </p>
    );
  }
  if (first === 'missing') {
    return (
      <p className="link-refused" role="alert">
        The sign-in link in the address of this page is not a Dudes sign-in link. To sign in, run <OpenCommand /> in a terminal.
      </p>
    );
  }
  if (first === 'expired') return <SignInExpired />;
  if (first === 'no_key') return <SignInExpired why="This browser no longer has its sign-in key." />;
  if (first === 'reload') {
    return (
      <p className="link-refused" role="alert">
        Reload this page to finish signing in.
      </p>
    );
  }
  if (first === 'unsupported') {
    return (
      <p className="link-refused" role="alert">
        This browser cannot keep a sign-in key, so it was not signed in. Some private windows block the storage
        it needs. Try a normal window.
      </p>
    );
  }
  if (!refused) return null;
  if (buildMismatch) {
    return (
      <p className="link-refused" role="alert">
        Dudes now serves a different Floor build from this page, so the link pasted into this tab was not
        used. This page is as it was, but it cannot sign in to the new build. Copy any draft you need, reload
        this tab, then paste the link again.
      </p>
    );
  }
  return (
    <p className="link-refused" role="alert">
      The Floor link pasted into this tab was refused, or Dudes could not be reached to take it. This page is
      as it was and keeps what it holds. For a new sign-in, run <OpenCommand /> in a terminal.
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
  const { signOutFailed } = useSyncExternalStore(subscribeToDevices, getDevicesState, getDevicesState);
  const signOut = () => { void signOutHere(); };
  const floor = useSyncExternalStore(subscribeToFloor, getFloorState, getFloorState);
  const listIsCurrent = floor.phase === 'live' && floor.snapshotEpoch === floor.epoch;
  useEffect(() => {
    if (listIsCurrent) void loadDevices();
  }, [listIsCurrent, floor.epoch]);
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [hiring, setHiring] = useState(false);
  const [guide, setGuide] = useState<'open' | 'closed'>('closed');
  const [archiveHireKept, setArchiveHireKept] = useState(false);
  const [officeBar, setOfficeBar] = useState<HTMLDivElement | null>(null);
  const hire = useSyncExternalStore(subscribeToHire, getHireState, getHireState);
  const page = useSyncExternalStore(subscribeToPage, currentPage, () => null);
  const [panel, setPanel] = useState<'overview' | 'board' | null>(null);
  const view = page ?? panel;
  const [answerAsked, setAnswerAsked] = useState<{ readonly id: string } | null>(null);
  const [recoveryAsked, setRecoveryAsked] = useState<{ readonly id: string } | null>(null);
  const [recoveryReceiptAsked, setRecoveryReceiptAsked] = useState<string | null>(null);
  const [crewOpen, setCrewOpen] = useState(true);
  const [focused, setFocused] = useState(false);
  const [preferences, setPreferences] = useState(readViewPreferences);
  const [osDark, setOsDark] = useState(() => typeof window !== 'undefined'
    && window.matchMedia('(prefers-color-scheme: dark)').matches);
  const drag = useRef<{ pointerId: number; x: number; width: number } | null>(null);
  const hireDialog = useRef<HTMLDialogElement | null>(null);
  const hireOpener = useRef<HTMLElement | null>(null);
  const backdropPress = useRef<number | null>(null);
  const shellRef = useRef<HTMLElement | null>(null);
  const [noticeStack, setNoticeStack] = useState<HTMLDivElement | null>(null);
  const [noticeArea] = useState(() => {
    if (typeof document === 'undefined') return null;
    const area = document.createElement('div');
    area.className = 'notice-area';
    return area;
  });
  const [noticeBanner] = useState(() => {
    if (typeof document === 'undefined') return null;
    const banner = document.createElement('section');
    banner.className = 'notice-banner';
    banner.setAttribute('aria-label', 'Floor status');
    return banner;
  });
  useLayoutEffect(() => {
    const shell = shellRef.current;
    if (shell === null || noticeArea === null || noticeBanner === null) return;
    const sync = () => {
      const pinned = noticeBanner.parentElement?.classList.contains('top-region') === true;
      shell.style.setProperty('--banner-height', `${pinned ? noticeBanner.offsetHeight : 0}px`);
    };
    const update = () => {
      const dialogs = shell.querySelectorAll<HTMLDialogElement>('.hire-dialog:modal, .retire-dialog:modal, .ticket-dialog:modal, .overview-dialog:modal');
      const modal = dialogs.item(dialogs.length - 1);
      const host = modal ?? shell;
      if (noticeArea.parentElement !== host) host.append(noticeArea);
      const top = shell.querySelector('.top-region');
      if (modal !== null) {
        if (noticeBanner.parentElement !== modal) modal.prepend(noticeBanner);
      } else if (top !== null && noticeBanner.parentElement !== top) top.append(noticeBanner);
      sync();
    };
    const observer = new MutationObserver(update);
    observer.observe(shell, { childList: true, subtree: true, attributes: true, attributeFilter: ['open'] });
    const height = new ResizeObserver(sync);
    height.observe(noticeBanner);
    update();
    return () => { observer.disconnect(); height.disconnect(); noticeArea.remove(); noticeBanner.remove(); };
  }, [noticeArea, noticeBanner]);
  const selectedRow = selectedAgentRow(floor.agents, selectedId, floor.phase, floor.stale);
  const focusedId = focused ? focusAgentId(floor, selectedId) : null;
  const displayRow = focused ? selectedAgentRow(floor.agents, focusedId, floor.phase, floor.stale) : selectedRow;
  const currentSelectedId = displayRow?.id ?? null;
  const selectable = floor.phase === 'live' && !floor.stale;
  const effectiveTheme = preferences.theme === 'system' ? (osDark ? 'dark' : 'light') : preferences.theme;
  const reveal = agentToReveal(hire, floor);
  const drawerOpen = displayRow !== null || floor.output.targetId !== null;
  const home = 'header-more-toggle';
  const drawerFocus = drawerHandBack({ agent: currentSelectedId, focused, crewOpen, viewOpen: view !== null });
  const leaveDrawerTo = ['crew-toggle', 'focus-toggle'].includes(drawerFocus) ? home : drawerFocus;

  useDesktopNotifications((target) => {
    setGuide('closed');
    if (hiring) closeHire('revealed');
    if (target.kind === 'ticket') openCard(target.id);
    else { setFocused(false); closeView(); setCrewOpen(true); openQuestion(target.id); }
  });

  useEffect(() => {
    if (page === null) return;
    setFocused(false);
    setPanel(null);
    setGuide('closed');
    if (hiring) {
      closeHire('navigation');
      document.getElementById(CONTROL_TITLE_ID)?.focus();
    }
    if (CONTROL_VIEWS.some((entry) => entry.view === page && entry.refresh)) refreshFloorSnapshot();
  }, [page]);

  // A new agent shows in Crew and the office but is not opened: its drawer would fill a phone screen and push the bar away.
  useEffect(() => {
    if (reveal === null) return;
    markRevealed(reveal);
    // With tickets off the hire's last screen says so and offers the switch, so it stays open.
    if (hire.outcome?.kind === 'complete' && !ticketsOff(floor)) {
      closeHire();
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
    if (recoveryReceiptAsked === null || drawerOpen) return;
    const receipt = document.getElementById(`recovery-receipt-${recoveryReceiptAsked}`);
    receipt?.focus();
    receipt?.scrollIntoView({ block: 'start' });
    setRecoveryReceiptAsked(null);
  }, [recoveryReceiptAsked, drawerOpen]);

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
        '.terminal-panel, [data-hire-wizard], .ticket-dialog, .first-run-guide, input, textarea, select',
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
    if (page !== null || view === 'overview') closeView();
    selectOutputAgent(id);
    setSelectedId(id);
    return true;
  }

  function openQuestion(id: string): void {
    if (selectAgent(id)) setAnswerAsked({ id });
  }

  function reviewRecovery(id: string): void {
    if (!selectAgent(id)) return;
    closeView();
    setRecoveryAsked({ id });
  }

  function readRecoveryReceipt(id: string): void {
    clearSelection();
    closeView();
    setRecoveryReceiptAsked(id);
  }

  function openView(next: ControlView): void {
    setFocused(false);
    if (isPageView(next)) {
      setPanel(null);
      openPage(next);
    } else {
      openPage(null);
      setPanel(next);
    }
  }

  function closeView(): void {
    setPanel(null);
    openPage(null);
  }

  function toggleView(next: ControlView): void {
    const target = toggledView(view, next, focused);
    if (target === null) closeView();
    else openView(target);
  }

  function toggleCrew(): void {
    if (page !== null || view === 'board') {
      closeView();
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

  function openSettingsFromHire(): void {
    closeHire('navigation');
    openView('settings');
    document.getElementById(CONTROL_TITLE_ID)?.focus();
  }

  function closeHire(reason: 'dismissed' | 'revealed' | 'navigation' = 'dismissed'): void {
    backdropPress.current = null;
    if (reason !== 'navigation') leaveHire();
    hireDialog.current?.close();
    setHiring(false);
    const ticketDialog = document.getElementById('selected-card-detail');
    const ticketFallback = ticketDialog?.getAttribute('open') != null
      ? ticketDialog.querySelector<HTMLElement>('.ticket-dialog-close') : null;
    if (reason === 'dismissed' || (reason === 'revealed' && ticketFallback !== null)) {
      restoreFocusAfterDialog(hireOpener.current,
        ticketFallback ?? document.getElementById('hire-open') ?? document.getElementById(home));
    }
  }

  useEffect(() => {
    if (!hiring) return;
    const dialog = hireDialog.current;
    dialog?.showModal();
    (dialog?.querySelector<HTMLElement>('#hire-name') ?? dialog?.querySelector<HTMLElement>('#hire-title'))?.focus();
  }, [hiring]);

  function clearSelection(): void {
    if (focused) setFocused(false);
    selectOutputAgent(null);
    setSelectedId(null);
  }

  function toggleFocus(): void {
    if (page !== null) closeView();
    if (!focused) {
      setHiring(false);
      const id = focusAgentId(floor, selectedId);
      if (id !== null) selectAgent(id);
    }
    setFocused((active) => !active);
    document.getElementById(home)?.focus();
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
    leaveFocusTo={leaveDrawerTo} recoveryRequested={recoveryAsked} onRecoveryShown={() => setRecoveryAsked(null)}
    onReadRecoveryReceipt={readRecoveryReceipt} onOpenTicket={openCard} />;

  const banner = (
    <>
      <JournalStoppedBanner stopped={floor.journalStopped} />
      <ClosingTimeBanner closing={floor.closingDisplay?.phase ?? null} progress={floor.closingDisplay?.progress ?? null} stale={floor.stale || floor.closingTime === null} />
      <StreamNotice floor={floor} placement="banner" />
      <DeviceNotices part="asking" />
    </>
  );
  const notices = (
    <NoticeStack host={setNoticeStack}>
      <PastedLinkNotice />
      {view === 'board' ? null : <TicketNotice floor={floor} />}
      <NotificationOffer onBoard={view === 'board'} leaveFocusTo="floor-notices" />
      {signOutFailed ? <p className="link-refused" role="alert">Could not sign out, so this browser is still signed in. Try again.</p> : null}
      {focused || floor.agents.length === 0 ? <TicketBanner floor={floor} onOpen={() => openView('board')} /> : null}
      <StreamNotice floor={floor} placement="stack" />
      <DeviceNotices part="gone" />
      <DoneNotices notices={floor.doneNotices} leaveFocusTo="floor-notices" />
    </NoticeStack>
  );

  return (
    <TicketNoticeHost value={noticeStack}><main className="shell" ref={shellRef} aria-labelledby="floor-title" data-focus={focused ? 'true' : 'false'}
      data-page={page ?? undefined}
      onKeyDown={(event) => {
        if (page !== null && closesPanel(event)) { event.stopPropagation(); closeView(); }
      }}
      data-theme={preferences.theme} data-scheme={effectiveTheme} style={{ '--side-width': `${preferences.sidebarWidth}px` } as CSSProperties}>
      <div className="top-region">
        <header className="top-bar">
          <h1 id="floor-title" className="wordmark">dudes</h1>
          <CrewStats agents={floor.agents} cards={floor.cards} statuses={focused} />
          <ControlNav floor={floor} view={focused ? null : view} onToggle={toggleView} />
          <div className="top-bar-actions" role="group" aria-label="Floor actions">
            {hireSwitchOn(floor) ? <button type="button" className="hire-open" id="hire-open" aria-expanded={hiring}
              aria-controls={hiring ? 'hire-wizard' : undefined}
              onClick={(event) => openHire(event.currentTarget)}>Hire an agent</button> : null}
            <PauseAllControl floor={floor} />
            <details className="header-more" onKeyDown={(event) => {
              if (event.key === 'Escape') {
                event.currentTarget.open = false;
                event.currentTarget.querySelector('summary')?.focus();
                event.stopPropagation();
              }
            }}>
              <summary id="header-more-toggle">More</summary>
              <div className="header-menu" onClick={(event) => {
                if (!(event.target as HTMLElement).closest('button')) return;
                const menu = event.currentTarget;
                const details = menu.closest('details');
                details?.removeAttribute('open');
                if (menu.contains(menu.ownerDocument.activeElement)) details?.querySelector('summary')?.focus();
              }}>
                {page === null ? null : <button type="button" id="office-open" onClick={closeView}>Office</button>}
                <button type="button" className="crew-toggle" id="crew-toggle" aria-pressed={page === null && crewOpen && !focused && view !== 'board'}
                  aria-controls={page === null && crewOpen && !focused && view !== 'board' ? 'crew-list' : undefined}
                  onClick={toggleCrew}>Crew</button>
                <button type="button" className="focus-toggle" id="focus-toggle" aria-pressed={focused}
                  aria-keyshortcuts="Alt+Shift+F" onClick={toggleFocus}>
                  {focused ? 'Leave focus' : 'Focus agent'}
                </button>
                <button type="button" className="theme-toggle" aria-pressed={effectiveTheme === 'dark'}
                  onClick={() => setPreferences((current) => ({ ...current, theme: toggleTheme(current.theme, osDark) }))}>
                  Use {effectiveTheme === 'dark' ? 'light' : 'dark'} theme
                </button>
                <button type="button" className="sign-out" onClick={signOut}>Sign out</button>
              </div>
            </details>
          </div>
          <div className="office-bar" ref={setOfficeBar} />
        </header>
      </div>
      <div className="floor-stage" data-page={page ?? undefined}
        onKeyDown={(event) => { if (closesDrawer(event, drawerOpen)) { event.stopPropagation(); clearSelection(); } }}>
        {crewOpen && !focused ? <Roster agents={floor.agents} cards={floor.cards} stepLabels={floor.stepLabels} selectedId={currentSelectedId}
          selectable={selectable} stale={floor.stale} onSelectAgent={selectAgent}
          onOpenQuestion={openQuestion} onOpenTicket={openCard} sheet
          addAgent={floor.agents.length === 0 && hireSwitchOn(floor) ? null
            : <HireEntry floor={floor} id="hire-open-crew" onAddAgent={openHire} folded />} /> : null}
        {view === null || view === 'overview' || focused ? null : <Control key={view} floor={floor} view={view}
          onClose={closeView} onView={openView}
          onReviewRecovery={reviewRecovery} onHire={openHire}
          onGuide={() => { setFocused(false); setHiring(false); setGuide('open'); }} />}
        <div className="office-slot">
          <div className="office-area">
            {page === null ? <OfficeCanvas floor={floor} selectedId={currentSelectedId} onSelectAgent={selectAgent} onOpenQuestion={openQuestion}
              onOpenTasks={() => openView('board')} onAddAgent={openHire} bar={officeBar} /> : null}
            <FirstTask floor={floor} onHire={openHire} />
          </div>
        </div>
        <div className="stage-center">
          <TerminalPanel floor={floor} theme={effectiveTheme} />
        </div>
        <div className="stage-side">
          <Receipts floor={floor} leaveFocusTo={displayRow === null ? home : AGENT_DETAIL_HEADING_ID} />
          {focused ? <section className="focus-mode" aria-label="Focus mode" ref={handFocusOnLeave}
            data-focus-on-leave="header-more-toggle">
            <h2>Focused agent</h2>
            <div className="focus-frame">
              {drawer}
              {splitter}
              <FocusRoster floor={floor} selectedId={currentSelectedId} onSelectAgent={selectAgent}
                onOpenQuestion={openQuestion} onOpenTicket={openCard}
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
          }} /> : null}
      </div>
      {view === 'overview' && !focused ? <OverviewDialog floor={floor} selectedId={currentSelectedId} onClose={closeView}
        onSelectAgent={selectAgent} onOpenQuestion={openQuestion} onOpenTicket={openCard} onHireAgain={hireAgain} /> : null}
      {noticeArea === null ? null : createPortal(notices, noticeArea)}
      {noticeBanner === null ? null : createPortal(banner, noticeBanner)}
      <RetireDialog floor={floor} />
      {hiring ? <Dialog size="lg" className="hire-dialog" ref={hireDialog}
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
        {archiveHireKept ? <div className="px-6 pt-4"><Notice tone="warn" role="status">The current Hire is kept. Resolve it before preparing another agent.</Notice></div> : null}
        <HireWizard floor={floor} onClose={() => closeHire()} onSettings={openSettingsFromHire} />
      </Dialog> : null}
    </main></TicketNoticeHost>
  );
}
