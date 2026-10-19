// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Attach in the selected agent's detail, and the one channel's screen.
//
// A view over two stores. The floor store says whether the operator turned
// the terminal on and what each agent's row says; the terminal store says
// where the one channel is. Nothing here reads the wire — `terminal.ts` does —
// and the only input this file hands on is what the screen received from the
// operator. With the terminal off, which is the default, it draws nothing.

import { useEffect, useRef, useState, useSyncExternalStore } from 'react';
import type { RefObject } from 'react';

import { currentFloorSession } from '../../session.ts';
import type { FloorState } from '../floor/store.ts';
import { browserSocket } from '../floor/stream.ts';
import { AGENT_DETAIL_HEADING_ID, handFocusOnLeave } from '../focus.ts';
import { UnavailableControls } from '../settings/SettingsView.tsx';
import { RetireStrip, openConfirmation } from '../retire/RetirePanel.tsx';
import type { AgentRow } from '../scene/cast.ts';
import type { Terminal } from './grid.ts';
import {
  MAX_FONT_SIZE, MIN_FONT_SIZE, copyTerminalSelection, fitTerminal, readFontSize, terminalShortcut,
  writeFontSize, zoomFontSize,
} from './controls.ts';
import type { CopyResult, TerminalCommand, TerminalPlatform } from './controls.ts';
import './controls.css';
import {
  LEAVE_KEY, abandonTerminal, attachTerminal, bindTerminalScreen, detachTerminal, dismissTerminal,
  getTerminalState, leavesTerminal, subscribeToTerminal, terminalOffer, writeTerminal,
} from './terminal.ts';
import type { TerminalChannel, TerminalEnding, TerminalOffer, TerminalPorts } from './terminal.ts';
import { lazyChunk } from '../../transport.ts';

type OpenChannel = Exclude<TerminalChannel, { readonly phase: 'idle' }>;

function browserTerminalPorts(): TerminalPorts {
  return { location: window.location, socket: browserSocket, csrf: () => currentFloorSession()?.csrf ?? null };
}

function nameOf(floor: FloorState, agent: string): string {
  const row = floor.agents.find((candidate) => candidate.id === agent);
  return typeof row?.name === 'string' && row.name !== '' ? row.name : agent;
}

const ATTACH_HINT = 'Attach hands you this agent’s CLI. Its loop holds while you type, and Detach hands it back.';

const UNAVAILABLE: Readonly<Record<Extract<TerminalOffer, { kind: 'unavailable' }>['reason'], string>> = Object.freeze({
  running: 'Attach waits for this agent’s running turn to end.',
  attached: 'A person already holds this agent’s keyboard.',
  halted: 'A halt stands on this agent, so the Floor cannot attach it.',
  'no-session': 'Attach waits for this agent’s first confirmed session.',
});

const TRUST_WARNING = 'Claude Code may ask you to trust this folder. Check the folder before accepting in the terminal. '
  + 'No, exit may be selected when the prompt opens.';

/** Claude Code's workspace trust prompt comes up inside the terminal, so it is read here first. */
export function AttachConfirmation({ name, onConfirm, onCancel }: {
  readonly name: string; readonly onConfirm: () => void; readonly onCancel: () => void;
}) {
  return (
    <dialog ref={openConfirmation} className="retire-dialog" data-attach-dialog
      aria-labelledby="attach-dialog-title" aria-describedby="attach-dialog-text" onCancel={onCancel}>
      <h2 id="attach-dialog-title">{`Attach to ${name}?`}</h2>
      <p id="attach-dialog-text">{TRUST_WARNING}</p>
      <div className="retire-actions">
        <button type="button" className="terminal-attach-confirm"
          onClick={(event) => { if (event.detail < 2) onConfirm(); }}>Attach</button>
        <button type="button" className="retire-cancel" onClick={onCancel}>Cancel</button>
      </div>
    </dialog>
  );
}

export function TerminalAttach({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow }) {
  const channel = useSyncExternalStore(subscribeToTerminal, getTerminalState, getTerminalState);
  return <TerminalAttachView floor={floor} offer={terminalOffer(floor, row, channel)} />;
}

function whyNoAttach(floor: FloorState, offer: Exclude<TerminalOffer, { readonly kind: 'none' | 'attach' }>): string {
  switch (offer.kind) {
    case 'open-here': return 'This agent’s terminal is open.';
    case 'open-elsewhere': return `The terminal is open for ${nameOf(floor, offer.agent)}. Detach it before attaching this agent.`;
    case 'unavailable': return UNAVAILABLE[offer.reason];
  }
}

/**
 * The one Attach button. Its words, or the reason it cannot be pressed, are its tooltip and its
 * description. While it waits it is aria-disabled, never disabled, so the reason stays reachable by keyboard.
 */
export function TerminalAttachView({ floor, offer }: { readonly floor: FloorState; readonly offer: TerminalOffer }) {
  const [confirming, setConfirming] = useState(false);
  if (offer.kind === 'none') return <UnavailableControls floor={floor} control="terminal" />;
  const target = offer.kind === 'attach' ? offer.agent : null;
  const hint = offer.kind === 'attach' ? ATTACH_HINT : whyNoAttach(floor, offer);
  const claudeTrust = target !== null && floor.agents.find((row) => row.id === target)?.provider === 'claude';
  return (
    <div className="agent-detail-terminal" data-terminal-offer={offer.kind}>
      <button type="button" className="terminal-attach" title={hint} aria-describedby="terminal-hint"
        aria-haspopup={claudeTrust ? 'dialog' : undefined} aria-disabled={target === null || undefined}
        onClick={() => { if (target !== null) { if (claudeTrust) setConfirming(true); else attachTerminal(target, browserTerminalPorts()); } }}>Attach</button>
      {confirming && target !== null && claudeTrust ? <AttachConfirmation name={nameOf(floor, target)}
        onCancel={() => setConfirming(false)}
        onConfirm={() => { setConfirming(false); attachTerminal(target, browserTerminalPorts()); }} /> : null}
      <span className="visually-hidden" id="terminal-hint">{hint}</span>
    </div>
  );
}

const PROGRESS: Readonly<Record<Exclude<OpenChannel['phase'], 'ended'>, string>> = Object.freeze({
  opening: 'Opening the terminal channel…',
  attaching: 'Attaching — waiting for the daemon…',
  attached: 'Attached — what you type goes to this agent’s CLI. Its loop holds until you detach.',
  detaching: 'Detaching — waiting for the daemon to confirm the close…',
});

const RETURNED = 'The daemon closes the agent’s terminal and returns the agent to its loop.';

const ENDINGS: Readonly<Record<Exclude<TerminalEnding, 'closed'>, string>> = Object.freeze({
  detached: 'Detached. The daemon confirmed the close, and the agent is back under its loop.',
  refused: 'The daemon refused to attach this agent. It may have started a turn, gained a halt or left the floor '
    + 'since this page read it.',
  unconfirmed: 'The daemon closed the terminal without confirming the detach. The agent stays held until the daemon '
    + 'confirms its close.',
  policy: `The daemon refused a message from this page and closed the terminal. ${RETURNED}`,
  lost: `The terminal channel was lost. ${RETURNED}`,
  unopened: 'The terminal channel did not open. The Floor session may have ended, or its one terminal slot is still '
    + 'closing; try Attach again.',
  unloaded: 'This page could not load its terminal, so it attached nothing. Reload the page to try again.',
});

/**
 * Why the daemon closed an attached channel. The close itself carries no
 * reason, so the projection is read for one: it may arrive after the close,
 * and the wording follows it when it does.
 */
function closedBecause(floor: FloorState, agent: string): string {
  if (floor.phase === 'closed') return 'The Floor session ended or the daemon stopped, so the terminal closed.';
  const row = floor.agents.find((candidate) => candidate.id === agent);
  const current = floor.phase === 'live' && floor.snapshotEpoch === floor.epoch;
  if (current && row === undefined) return 'The agent was retired, so the daemon closed its terminal.';
  if (row?.halted === true) return 'A halt now stands on this agent, so the daemon closed its terminal.';
  return `The daemon closed the terminal: the Floor session may have expired, or the agent’s CLI exited. ${RETURNED}`;
}

function statusOf(channel: OpenChannel, floor: FloorState): string {
  if (channel.phase !== 'ended') return PROGRESS[channel.phase];
  return channel.ending === 'closed' ? closedBecause(floor, channel.agent) : ENDINGS[channel.ending];
}

/** The one channel's screen, from Attach until the operator closes it. */
export function TerminalPanel({ floor, theme = 'light' }: {
  readonly floor: FloorState;
  readonly theme?: 'light' | 'dark';
}) {
  const channel = useSyncExternalStore(subscribeToTerminal, getTerminalState, getTerminalState);
  if (channel.phase === 'idle') return null;
  return <TerminalPanelView key={channel.id} channel={channel} floor={floor} theme={theme} />;
}

const KEYS_HINT = `Tab and Shift+Tab go to the CLI. Press ${LEAVE_KEY} to move the keyboard from the terminal to `
  + 'this panel’s button.';

export function TerminalPanelView({ channel, floor, theme = 'light' }: {
  readonly channel: OpenChannel;
  readonly floor: FloorState;
  readonly theme?: 'light' | 'dark';
}) {
  const actions = useRef<HTMLDivElement>(null);
  const screen = useRef<Terminal | null>(null);
  const [fontSize, setFontSize] = useState(readFontSize);
  const [expanded, setExpanded] = useState(false);
  const [copy, setCopy] = useState<CopyResult | null>(null);
  const [platform] = useState<TerminalPlatform>(() => typeof navigator !== 'undefined'
    && /Mac|iPhone|iPad|iPod/.test(navigator.platform) ? 'mac' : 'other');
  const modifier = platform === 'mac' ? 'Meta' : 'Control+Shift';
  const shortcuts = platform === 'mac'
    ? 'Cmd+= zooms in, Cmd+- zooms out, Cmd+0 resets, and Cmd+C copies the selection.'
    : 'Press Ctrl+] first, then Ctrl+Shift+= to zoom in, Ctrl+Shift+- to zoom out, Ctrl+Shift+0 to reset, '
      + 'or Ctrl+Shift+C to copy the selection. These shortcuts work only from this panel’s buttons.';

  useEffect(() => { writeFontSize(fontSize); }, [fontSize]);

  function command(action: TerminalCommand): void {
    if (action === 'copy') {
      void copyTerminalSelection(screen.current).then(setCopy);
    } else if (action === 'expand') {
      const focused = document.activeElement;
      setExpanded((current) => !current);
      requestAnimationFrame(() => {
        if (focused instanceof HTMLElement && focused.isConnected) focused.focus();
      });
    } else {
      setFontSize((current) => zoomFontSize(current, action));
    }
  }

  const ended = channel.phase === 'ended';
  const failed = ended && channel.ending !== 'detached';
  const detaching = channel.phase === 'detaching';
  return (
    <section className="terminal-panel" aria-labelledby="terminal-title" data-terminal-agent-id={channel.agent}
      data-terminal-phase={channel.phase} data-terminal-ending={ended ? channel.ending : undefined}
      data-terminal-expanded={expanded ? 'true' : 'false'}>
      <header className="terminal-head">
        <div>
          <p className="agent-detail-kicker">Terminal</p>
          <h2 id="terminal-title">{nameOf(floor, channel.agent)}</h2>
        </div>
        <div ref={actions} className="terminal-actions" onKeyDown={(event) => {
          if (expanded && event.key === 'Tab' && !event.ctrlKey && !event.metaKey && !event.altKey) {
            const buttons = actions.current?.querySelectorAll<HTMLButtonElement>('button:not(:disabled)');
            if (event.shiftKey && event.target === buttons?.[0]) {
              event.preventDefault();
              event.stopPropagation();
              screen.current?.focus();
              return;
            }
            if (!event.shiftKey && screen.current === null && buttons?.length
              && event.target === buttons[buttons.length - 1]) {
              event.preventDefault();
              event.stopPropagation();
              buttons[0]?.focus();
              return;
            }
          }
          const action = terminalShortcut(event, platform, 'header');
          if (action === null) return;
          event.preventDefault();
          event.stopPropagation();
          command(action);
        }}>
          <button type="button" aria-label="Zoom out" aria-keyshortcuts={`${modifier}+-`}
            disabled={fontSize <= MIN_FONT_SIZE} onClick={() => command('zoom-out')}>−</button>
          <button type="button" aria-label="Reset terminal zoom" aria-keyshortcuts={`${modifier}+0`}
            onClick={() => command('zoom-reset')}>{fontSize}px</button>
          <button type="button" aria-label="Zoom in" aria-keyshortcuts={`${modifier}+=`}
            disabled={fontSize >= MAX_FONT_SIZE} onClick={() => command('zoom-in')}>+</button>
          <button type="button" aria-label="Copy selection" aria-keyshortcuts={`${modifier}+C`}
            onClick={() => command('copy')}>Copy</button>
          <button type="button" aria-expanded={expanded} onClick={() => command('expand')}>
            {expanded ? 'Restore' : 'Expand'}</button>
          {channel.phase === 'attached' || detaching ? (
            <button type="button" className="terminal-detach" aria-disabled={detaching || undefined}
              aria-busy={detaching} onClick={detachTerminal}>{detaching ? 'Detaching…' : 'Detach'}</button>
          ) : null}
          {ended ? (
            <button ref={handFocusOnLeave} type="button" className="terminal-close"
              data-focus-on-leave={AGENT_DETAIL_HEADING_ID} onClick={dismissTerminal}>Close</button>
          ) : null}
          <RetireStrip floor={floor} row={floor.agents.find((row) => row.id === channel.agent) ?? null} />
        </div>
      </header>
      <p className={`terminal-state${ended ? ' terminal-ended' : ''}${failed ? ' terminal-failed' : ''}`}
        role={failed ? 'alert' : 'status'}>
        {statusOf(channel, floor)}
      </p>
      <p className="hire-note terminal-keys">{KEYS_HINT}</p>
      <p className="hire-note terminal-shortcuts">{shortcuts}</p>
      {copy === null ? null : <div className="terminal-copy-result">
        <p role="status">{copy.kind === 'copied' ? 'Selection copied.'
          : copy.kind === 'empty' ? 'Select terminal text before copying.'
            : 'The browser refused clipboard access. Select the text below and use your browser’s Copy command.'}</p>
        {copy.kind === 'refused' ? <textarea readOnly aria-label="Terminal selection to copy"
          value={copy.selection} onFocus={(event) => event.currentTarget.select()} /> : null}
      </div>}
      <TerminalScreen phase={channel.phase} leaveTo={actions} theme={theme} screen={screen}
        fontSize={fontSize} platform={platform} onCommand={command} />
    </section>
  );
}

/** Input only while attached, and the keyboard goes to the grid when it is. */
function follow(term: Terminal, phase: TerminalChannel['phase']): void {
  term.options.disableStdin = phase !== 'attached';
  if (phase === 'attached') term.focus();
}

function terminalTheme(element: HTMLElement) {
  const tokens = getComputedStyle(element);
  const color = (name: string) => tokens.getPropertyValue(name).trim();
  return {
    background: color('--terminal-background'), foreground: color('--terminal-foreground'),
    cursor: color('--terminal-cursor'), cursorAccent: color('--terminal-background'),
    selectionBackground: color('--terminal-selection'),
  };
}

/**
 * The grid itself: output as the daemon sent it, and the operator's own
 * input. Its code loads only when a terminal opens, so a Floor with the
 * terminal off never fetches it.
 */
function TerminalScreen({ phase, leaveTo, theme, screen, fontSize, platform, onCommand }: {
  readonly phase: OpenChannel['phase'];
  /** Where `LEAVE_KEY` sends the keyboard: the first button in it. */
  readonly leaveTo: RefObject<HTMLElement | null>;
  readonly theme: 'light' | 'dark';
  readonly screen: RefObject<Terminal | null>;
  readonly fontSize: number;
  readonly platform: TerminalPlatform;
  readonly onCommand: (command: TerminalCommand) => void;
}) {
  const host = useRef<HTMLDivElement>(null);
  const current = useRef({ fontSize, platform, onCommand });
  current.current = { fontSize, platform, onCommand };

  useEffect(() => {
    const element = host.current;
    if (element === null) return undefined;
    let release = () => {};
    let gone = false;
    void lazyChunk(() => import('./grid.ts')).then(({ Terminal }) => {
      if (gone) return;
      const term = new Terminal({
        fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
        fontSize: current.current.fontSize,
        theme: terminalTheme(element),
        disableStdin: true,
      });
      term.open(element);
      term.attachCustomKeyEventHandler((event) => {
        if (leavesTerminal(event)) {
          if (event.type === 'keydown') {
            event.preventDefault();
            leaveTo.current?.querySelector<HTMLButtonElement>('button:not(:disabled)')?.focus();
          }
          return false;
        }
        const action = terminalShortcut(event, current.current.platform, 'grid');
        if (action === null) return true;
        if (event.type === 'keydown') {
          event.preventDefault();
          event.stopPropagation();
          current.current.onCommand(action);
        }
        return false;
      });
      const fit = () => fitTerminal(term, element);
      fit();
      // Besides typing and paste, onData carries the grid's answers to the
      // program's own terminal queries, as any terminal's input does. The page
      // adds nothing.
      const input = term.onData((data) => writeTerminal(data));
      const unbind = bindTerminalScreen({ size: () => ({ cols: term.cols, rows: term.rows }), write: (bytes) => term.write(bytes) });
      const observer = new ResizeObserver(fit);
      observer.observe(element);
      screen.current = term;
      follow(term, getTerminalState().phase);
      release = () => {
        observer.disconnect();
        unbind();
        input.dispose();
        screen.current = null;
        term.dispose();
      };
    }, () => {
      if (!gone) abandonTerminal();
    });
    return () => {
      gone = true;
      release();
    };
  }, []);

  useEffect(() => {
    if (screen.current !== null && host.current !== null) {
      screen.current.options.fontSize = fontSize;
      fitTerminal(screen.current, host.current);
    }
  }, [fontSize]);

  useEffect(() => {
    if (screen.current !== null) follow(screen.current, phase);
  }, [phase]);

  useEffect(() => {
    if (screen.current !== null && host.current !== null) {
      screen.current.options.theme = terminalTheme(host.current);
    }
  }, [theme]);

  return <div ref={host} className="terminal-screen" />;
}
