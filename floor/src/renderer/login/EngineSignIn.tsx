// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Sign in to an engine without leaving the page.
//
// A view over the login store. The hire flow says which engine is not signed
// in; this offers the one button that runs that engine's own login in a
// terminal the daemon owns, and draws the terminal in place. The page picks
// only the engine. It never shows or types a command.

import { useEffect, useRef, useSyncExternalStore } from 'react';

import { currentFloorSession } from '../../session.ts';
import { browserSocket, refreshFloorSnapshot } from '../floor/stream.ts';
import { HIRE_ENGINES } from '../onboarding/catalog.ts';
import type { HireEngine } from '../onboarding/catalog.ts';
import type { ProviderRow } from '../setup/daemon.ts';
import { fitGrid } from '../terminal/terminal.ts';
import type { Terminal } from '../terminal/grid.ts';
import { lazyChunk } from '../../transport.ts';
import {
  abandonLogin, bindLoginScreen, closeLogin, getLoginState, resizeLogin, startLogin, subscribeToLogin, writeLogin,
} from './login.ts';
import type { LoginChannel, LoginEnding, LoginPorts } from './login.ts';

type OpenChannel = Exclude<LoginChannel, { readonly phase: 'idle' }>;

function browserLoginPorts(): LoginPorts {
  return { location: window.location, socket: browserSocket, csrf: () => currentFloorSession()?.csrf ?? null };
}

/** What the person should do in the terminal, by engine. The daemon decides nothing from this. */
const STEPS: Readonly<Record<HireEngine, string>> = Object.freeze({
  claude: 'Open the link below in your browser and sign in. Paste the code it shows into this window, then press Enter.',
  codex: 'Open the link below in your browser and sign in. This window finishes by itself.',
  pi: 'Type /login, choose where to sign in, and follow the steps. Close this window when you are done.',
});

const PROGRESS: Readonly<Record<'opening' | 'starting', string>> = Object.freeze({
  opening: 'Opening the sign-in…',
  starting: 'Starting the sign-in…',
});

const ENDINGS: Readonly<Record<LoginEnding, string>> = Object.freeze({
  succeeded: 'Signed in. Checking again…',
  failed: 'The sign-in did not finish.',
  timed_out: 'The sign-in ran out of time after 10 minutes.',
  replaced: 'Another sign-in for this app took its place.',
  not_installed: 'Dudes could not find this app on this computer.',
  unavailable: 'This app cannot be signed in from the Floor on this computer.',
  could_not_start: 'The sign-in could not start.',
  needs_terminal: 'Signing in to Pi needs the Floor\u2019s terminal turned on in Settings.',
  lost: 'The connection to the sign-in was lost.',
  unopened: 'The sign-in did not open. The Floor session may have ended, so reload this page.',
  unloaded: 'This page could not load its terminal. Reload the page.',
});

function endingText(engine: HireEngine, ending: LoginEnding): string {
  return engine === 'pi' && ending === 'succeeded' ? 'Pi closed.' : ENDINGS[ending];
}

/** Whether the hire flow should offer to sign in to this engine. */
export function signInOffered(engine: HireEngine, row: ProviderRow | null): boolean {
  return row?.found === true && (row.signIn === 'signed_out' || engine === 'pi');
}

export function EngineSignIn({ engine, row, terminal }: {
  readonly engine: HireEngine;
  readonly row: ProviderRow | null;
  readonly terminal: boolean;
}) {
  const channel = useSyncExternalStore(subscribeToLogin, getLoginState, getLoginState);
  return <EngineSignInView engine={engine} row={row} channel={channel} terminal={terminal} />;
}

export function EngineSignInView({ engine, row, channel, terminal }: {
  readonly engine: HireEngine;
  readonly row: ProviderRow | null;
  readonly channel: LoginChannel;
  /** Whether the Floor's terminal is on, which Pi's sign-in needs. */
  readonly terminal: boolean;
}) {
  const name = HIRE_ENGINES[engine];
  if (channel.phase !== 'idle' && channel.engine === engine) {
    return <LoginPanel key={channel.id} engine={engine} channel={channel} />;
  }
  if (!signInOffered(engine, row)) return null;
  if (engine === 'pi' && !terminal) {
    return (
      <div className="login-offer" data-login-offer="pi">
        <p className="hire-note" role="status">{ENDINGS.needs_terminal}</p>
        <button type="button" className="login-recheck" onClick={() => refreshFloorSnapshot()}>Check again</button>
      </div>
    );
  }
  const busy = channel.phase !== 'idle' && channel.phase !== 'ended';
  return (
    <div className="login-offer" data-login-offer={engine}>
      {engine === 'pi' ? <p className="hire-note">
        Pi cannot tell Dudes whether it is signed in. If it needs a sign-in, open it here.
      </p> : null}
      <button type="button" className="login-start" aria-disabled={busy || undefined}
        onClick={() => { if (!busy) startLogin(engine, browserLoginPorts()); }}>
        {engine === 'pi' ? 'Open Pi to sign in' : `Sign in to ${name}`}
      </button>
    </div>
  );
}

function LoginPanel({ engine, channel }: { readonly engine: HireEngine; readonly channel: OpenChannel }) {
  const ended = channel.phase === 'ended';
  const failed = ended && channel.ending !== 'succeeded';
  const progress = channel.phase === 'opening' || channel.phase === 'starting' ? PROGRESS[channel.phase] : null;
  return (
    <section className="login-panel" aria-label={`Sign in to ${HIRE_ENGINES[engine]}`}
      data-login-phase={channel.phase} data-login-ending={ended ? channel.ending : undefined}>
      <p className={`login-state${failed ? ' login-failed' : ''}`} role={failed ? 'alert' : 'status'}>
        {ended ? endingText(engine, channel.ending) : progress ?? STEPS[engine]}
      </p>
      <LoginScreen phase={channel.phase} />
      <div className="login-actions">
        {failed ? <button type="button" className="login-retry"
          onClick={() => startLogin(engine, browserLoginPorts())}>Try again</button> : null}
        <button type="button" className="login-close" onClick={closeLogin}>Close</button>
      </div>
    </section>
  );
}

function themeOf(element: HTMLElement) {
  const tokens = getComputedStyle(element);
  const color = (name: string) => tokens.getPropertyValue(name).trim();
  return {
    background: color('--terminal-background'), foreground: color('--terminal-foreground'),
    cursor: color('--terminal-cursor'), cursorAccent: color('--terminal-background'),
    selectionBackground: color('--terminal-selection'),
  };
}

function fit(term: Terminal, host: HTMLElement): void {
  const drawn = term.element?.querySelector('.xterm-screen')?.getBoundingClientRect();
  if (drawn === undefined) return;
  const size = fitGrid(host.getBoundingClientRect(),
    { cols: term.cols, rows: term.rows, width: drawn.width, height: drawn.height });
  if (size !== null && (size.cols !== term.cols || size.rows !== term.rows)) term.resize(size.cols, size.rows);
  resizeLogin();
}

/**
 * The grid itself: output as the daemon sent it, and the person's own input.
 * Its code loads only when a sign-in opens.
 */
function LoginScreen({ phase }: { readonly phase: OpenChannel['phase'] }) {
  const host = useRef<HTMLDivElement>(null);
  const screen = useRef<Terminal | null>(null);

  useEffect(() => {
    const element = host.current;
    if (element === null) return undefined;
    let release = () => {};
    let gone = false;
    void lazyChunk(() => import('../terminal/grid.ts')).then(({ Terminal }) => {
      if (gone) return;
      const term = new Terminal({
        fontFamily: 'ui-monospace, SFMono-Regular, Menlo, Consolas, monospace',
        fontSize: 14,
        theme: themeOf(element),
        disableStdin: true,
      });
      term.open(element);
      fit(term, element);
      // Besides typing and paste, onData carries the grid's answers to the
      // program's own terminal queries, as any terminal's input does. The page
      // adds nothing.
      const input = term.onData((data) => writeLogin(data));
      const unbind = bindLoginScreen({ size: () => ({ cols: term.cols, rows: term.rows }), write: (bytes) => term.write(bytes) });
      const observer = new ResizeObserver(() => fit(term, element));
      observer.observe(element);
      screen.current = term;
      term.options.disableStdin = getLoginState().phase !== 'running';
      release = () => {
        observer.disconnect();
        unbind();
        input.dispose();
        screen.current = null;
        term.dispose();
      };
    }, () => {
      if (!gone) abandonLogin();
    });
    return () => {
      gone = true;
      release();
    };
  }, []);

  useEffect(() => {
    if (screen.current === null) return;
    screen.current.options.disableStdin = phase !== 'running';
    if (phase === 'running') screen.current.focus();
  }, [phase]);

  return <div ref={host} className="terminal-screen login-screen" />;
}
