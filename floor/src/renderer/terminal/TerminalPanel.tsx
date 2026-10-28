// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Attach in the selected agent's detail, and the one channel's screen.
//
// A view over two stores. The floor store says whether the operator turned
// the terminal on and what each agent's row says; the terminal store says
// where the one channel is. Nothing here reads the wire — `terminal.ts` does —
// and the only input this file hands on is what the screen received from the
// operator. With the terminal off, it explains how to turn it on.

import { Terminal as TerminalIcon } from 'lucide-react';
import { useEffect, useRef, useState, useSyncExternalStore } from 'react';
import type { RefObject } from 'react';

import { currentFloorSession } from '../../session.ts';
import { Button, Dialog, ICON, Textarea, cn } from '../../ui/index.ts';
import type { FloorState } from '../floor/store.ts';
import { browserSocket } from '../floor/stream.ts';
import { AGENT_DETAIL_HEADING_ID, handFocusOnLeave } from '../focus.ts';
import { UnavailableControls } from '../settings/SettingsView.tsx';
import { NOTE, ACTIONS, KICKER as KICKER_TEXT } from '../receipt.ts';
import { RetireStrip, openConfirmation } from '../retire/RetirePanel.tsx';
import { modelView } from '../model/model.ts';
import type { AgentRow } from '../scene/cast.ts';
import type { Terminal } from './grid.ts';
import {
  MAX_FONT_SIZE, MIN_FONT_SIZE, copyTerminalSelection, fitTerminal, readFontSize, terminalShortcut,
  writeFontSize, zoomFontSize,
} from './controls.ts';
import type { CopyResult, TerminalCommand, TerminalPlatform } from './controls.ts';
import {
  LEAVE_KEY, abandonTerminal, attachTerminal, bindTerminalScreen, detachTerminal, dismissTerminal,
  getTerminalState, leavesTerminal, subscribeToTerminal, terminalOffer, writeTerminal,
} from './terminal.ts';
import type { TerminalChannel, TerminalEnding, TerminalOffer, TerminalPorts } from './terminal.ts';
import { floorSigner, socketOffer } from '../../signer.ts';
import { lazyChunk } from '../../transport.ts';

type OpenChannel = Exclude<TerminalChannel, { readonly phase: 'idle' }>;

function browserTerminalPorts(): TerminalPorts {
  return { location: window.location, socket: browserSocket, signedIn: () => currentFloorSession() !== null, offer: socketOffer(floorSigner()) };
}

function nameOf(floor: FloorState, agent: string): string {
  const row = floor.agents.find((candidate) => candidate.id === agent);
  return typeof row?.name === 'string' && row.name !== '' ? row.name : agent;
}

const ATTACH_HINT = 'Open this agent’s real CLI. Automatic work waits while you type. Detach returns control to Dudes.';

const UNAVAILABLE: Readonly<Record<Extract<TerminalOffer, { kind: 'unavailable' }>['reason'], string>> = Object.freeze({
  engine: 'Opening this engine’s own terminal is not supported yet.',
  running: 'Wait for this agent’s current work to finish before opening its CLI.',
  attached: 'Someone already has this agent’s CLI open.',
  halted: 'This agent is halted, so its CLI cannot open. Press Resume first if you stopped it from this page.',
  'no-session': 'This agent does not have a saved CLI session to open yet.',
});

const TRUST_WARNING = 'Claude Code may ask you to trust this folder. Check the folder before accepting in the terminal. '
  + 'No, exit may be selected when the prompt opens.';

/** Claude Code's workspace trust prompt comes up inside the terminal, so it is read here first. */
export function AttachConfirmation({ name, model = null, onConfirm, onCancel }: {
  readonly name: string; readonly model?: string | null; readonly onConfirm: () => void; readonly onCancel: () => void;
}) {
  return (
    <Dialog ref={openConfirmation} size="sm" className="retire-dialog" data-attach-dialog
      aria-labelledby="attach-dialog-title" aria-describedby="attach-dialog-text" onCancel={onCancel}>
      <div className="grid gap-4 p-6">
        <h2 id="attach-dialog-title" className="m-0 font-sans text-title font-semibold text-ink">{`Attach to ${name}?`}</h2>
        <p id="attach-dialog-text" className="m-0 text-body text-muted">{TRUST_WARNING}</p>
        <p className="attach-model m-0 text-body text-muted" data-attach-model={model ?? ''}>
          {model === null ? 'The terminal opens on Claude’s default model.' : `The terminal opens on ${model}.`}
        </p>
        <div className={cn('retire-actions justify-end', ACTIONS)}>
          <Button className="retire-cancel" onClick={onCancel}>Cancel</Button>
          <Button variant="primary" className="terminal-attach-confirm"
            onClick={(event) => { if (event.detail < 2) onConfirm(); }}>Attach</Button>
        </div>
      </div>
    </Dialog>
  );
}

export function TerminalAttach({ floor, row }: { readonly floor: FloorState; readonly row: AgentRow }) {
  const channel = useSyncExternalStore(subscribeToTerminal, getTerminalState, getTerminalState);
  return <TerminalAttachView floor={floor} offer={terminalOffer(floor, row, channel)} />;
}

function whyNoAttach(floor: FloorState, offer: Exclude<TerminalOffer, { readonly kind: 'none' | 'attach' }>): string {
  switch (offer.kind) {
    case 'open-here': return 'This agent’s CLI is already open.';
    case 'open-elsewhere': return `The CLI is open for ${nameOf(floor, offer.agent)}. Detach it before opening this agent’s CLI.`;
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
  const targetRow = target === null ? undefined : floor.agents.find((row) => row.id === target);
  const claudeTrust = targetRow?.provider === 'claude';
  return (
    <div className="agent-detail-terminal flex flex-wrap items-center gap-x-3 gap-y-1" data-terminal-offer={offer.kind}>
      <Button className="terminal-attach" title={hint} aria-describedby="terminal-hint" icon={<TerminalIcon {...ICON} />}
        aria-haspopup={claudeTrust ? 'dialog' : undefined} aria-disabled={target === null || undefined}
        onClick={() => { if (target !== null) { if (claudeTrust) setConfirming(true); else attachTerminal(target, browserTerminalPorts()); } }}>Open the real CLI</Button>
      {confirming && target !== null && targetRow !== undefined && claudeTrust ? <AttachConfirmation name={nameOf(floor, target)}
        model={modelView(floor, targetRow).next}
        onCancel={() => setConfirming(false)}
        onConfirm={() => { setConfirming(false); attachTerminal(target, browserTerminalPorts()); }} /> : null}
      <span className={offer.kind === 'unavailable' && offer.reason === 'engine' ? 'agent-attach-note text-small text-muted' : 'sr-only'} id="terminal-hint">{hint}</span>
    </div>
  );
}

const PROGRESS: Readonly<Record<Exclude<OpenChannel['phase'], 'ended'>, string>> = Object.freeze({
  opening: 'Opening the CLI…',
  attaching: 'Waiting for Dudes to open the CLI…',
  attached: 'What you type goes to this agent’s CLI. Automatic work waits until you detach.',
  detaching: 'Closing the CLI. Waiting for Dudes to confirm…',
});

const RETURNED = 'Dudes closes the agent’s CLI and returns it to automatic work.';

const ENDINGS: Readonly<Record<Exclude<TerminalEnding, 'closed'>, string>> = Object.freeze({
  detached: 'Detached. Dudes confirmed the CLI closed. Automatic work can continue.',
  refused: 'Dudes could not open this agent’s CLI. It may have started work, been halted or left the Floor '
    + 'since this page last updated.',
  unconfirmed: 'The CLI connection closed before Dudes confirmed the detach. Automatic work stays paused until Dudes '
    + 'confirms the CLI closed.',
  policy: `Dudes could not accept a request from this page and closed the CLI. ${RETURNED}`,
  lost: `The CLI connection was lost. ${RETURNED}`,
  unopened: 'The CLI connection did not open. Your Floor sign-in may have ended, or another terminal may still be '
    + 'closing. Try Open the real CLI again.',
  unloaded: 'This page could not load the terminal, so no CLI opened. Reload the page to try again.',
});

/**
 * Why the daemon closed an attached channel. The close itself carries no
 * reason, so the projection is read for one: it may arrive after the close,
 * and the wording follows it when it does.
 */
function closedBecause(floor: FloorState, agent: string): string {
  if (floor.phase === 'closed') return 'Your Floor sign-in ended or Dudes stopped, so the CLI closed.';
  const row = floor.agents.find((candidate) => candidate.id === agent);
  const current = floor.phase === 'live' && floor.snapshotEpoch === floor.epoch;
  if (current && row === undefined) return 'The agent was retired, so Dudes closed its CLI.';
  if (row?.halted === true) {
    return row.halted_by_floor === true ? 'You stopped this agent, so Dudes closed its CLI. Press Resume to let it work again.'
      : 'This agent is halted, so Dudes closed its CLI.';
  }
  return `Dudes closed the CLI. Your Floor sign-in may have expired, or the agent’s CLI exited. ${RETURNED}`;
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
    <section className="terminal-panel relative z-(--layer-terminal) max-[801px]:data-[terminal-expanded=true]:h-screen box-border flex min-h-0 flex-1 flex-col gap-3 rounded-lg border border-line bg-surface p-3 font-sans text-ink" aria-labelledby="terminal-title" data-terminal-agent-id={channel.agent}
      data-terminal-phase={channel.phase} data-terminal-ending={ended ? channel.ending : undefined}
      data-terminal-expanded={expanded ? 'true' : 'false'}>
      <header className="terminal-head flex flex-wrap items-center justify-between gap-x-3 gap-y-2">
        <div>
          <p className={cn('agent-detail-kicker m-0', KICKER_TEXT)}>Terminal</p>
          <h2 id="terminal-title" className="m-0 text-title font-semibold">{nameOf(floor, channel.agent)}</h2>
        </div>
        <div ref={actions} className="terminal-actions flex flex-wrap items-center gap-1.5" onKeyDown={(event) => {
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
          <Button size="sm" aria-label="Zoom out" aria-keyshortcuts={`${modifier}+-`}
            aria-disabled={fontSize <= MIN_FONT_SIZE || undefined}
            title={fontSize <= MIN_FONT_SIZE ? `Minimum zoom is ${MIN_FONT_SIZE}px.` : undefined}
            aria-description={fontSize <= MIN_FONT_SIZE ? `Minimum zoom is ${MIN_FONT_SIZE}px.` : undefined} onClick={() => command('zoom-out')}>−</Button>
          <Button size="sm" aria-label="Reset terminal zoom" aria-keyshortcuts={`${modifier}+0`}
            onClick={() => command('zoom-reset')}>{fontSize}px</Button>
          <Button size="sm" aria-label="Zoom in" aria-keyshortcuts={`${modifier}+=`}
            aria-disabled={fontSize >= MAX_FONT_SIZE || undefined}
            title={fontSize >= MAX_FONT_SIZE ? `Maximum zoom is ${MAX_FONT_SIZE}px.` : undefined}
            aria-description={fontSize >= MAX_FONT_SIZE ? `Maximum zoom is ${MAX_FONT_SIZE}px.` : undefined} onClick={() => command('zoom-in')}>+</Button>
          <Button size="sm" aria-label="Copy selection" aria-keyshortcuts={`${modifier}+C`}
            onClick={() => command('copy')}>Copy</Button>
          <Button size="sm" aria-expanded={expanded} onClick={() => command('expand')}>
            {expanded ? 'Restore' : 'Expand'}</Button>
          {channel.phase === 'attached' || detaching ? (
            <Button size="sm" className="terminal-detach" aria-disabled={detaching || undefined}
              loading={detaching} aria-describedby={detaching ? 'terminal-status' : undefined} onClick={detachTerminal}>{detaching ? 'Detaching…' : 'Detach'}</Button>
          ) : null}
          {ended ? (
            <Button ref={handFocusOnLeave} size="sm" variant="primary" className="terminal-close"
              data-focus-on-leave={AGENT_DETAIL_HEADING_ID} onClick={dismissTerminal}>Close</Button>
          ) : null}
          <RetireStrip floor={floor} row={floor.agents.find((row) => row.id === channel.agent) ?? null} />
        </div>
      </header>
      <p className={cn('terminal-state m-0 rounded-md px-3 py-2 text-base text-ink',
        ended && 'terminal-ended bg-ok-soft font-medium', failed && 'terminal-failed bg-danger-soft')}
        id="terminal-status" role={failed ? 'alert' : 'status'}>
        {statusOf(channel, floor)}
      </p>
      <p className={cn('terminal-keys', NOTE)}>{KEYS_HINT}</p>
      <p className={cn('terminal-shortcuts', NOTE)}>{shortcuts}</p>
      {copy === null ? null : <div className="terminal-copy-result grid gap-2">
        <p role="status" className="m-0 text-base">{copy.kind === 'copied' ? 'Selection copied.'
          : copy.kind === 'empty' ? 'Select terminal text before copying.'
            : 'The browser refused clipboard access. Select the text below and use your browser’s Copy command.'}</p>
        {copy.kind === 'refused' ? <Textarea readOnly aria-label="Terminal selection to copy"
          value={copy.selection} className="max-h-20" onFocus={(event) => event.currentTarget.select()} /> : null}
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

  return <div ref={host} className="terminal-screen min-h-0 flex-1 overflow-hidden rounded-md bg-(--terminal-background)" />;
}
