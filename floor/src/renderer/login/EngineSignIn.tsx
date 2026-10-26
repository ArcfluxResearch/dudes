// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Sign in to an engine without leaving the page.
//
// A view over the login store. The hire flow says which engine is not signed
// in; this offers the one button that runs that engine's own login in a
// terminal the daemon owns, and draws the terminal in place. The page picks
// only the engine. It never shows or types a command.

import { useEffect, useRef, useState, useSyncExternalStore } from 'react';

import { Button, Input, Notice, RING, buttonClass, cn } from '../../ui/index.ts';
import { currentFloorSession } from '../../session.ts';
import { browserSocket, refreshFloorSnapshot } from '../floor/stream.ts';
import { HIRE_ENGINES } from '../onboarding/catalog.ts';
import type { HireEngine } from '../onboarding/catalog.ts';
import type { ProviderRow } from '../setup/daemon.ts';
import { fitGrid } from '../terminal/terminal.ts';
import type { Terminal } from '../terminal/grid.ts';
import { floorSigner, socketOffer } from '../../signer.ts';
import { lazyChunk } from '../../transport.ts';
import {
  abandonLogin, bindLoginScreen, closeLogin, getLoginState, resizeLogin, startLogin, subscribeToLogin, writeLogin,
} from './login.ts';
import type { LoginChannel, LoginEnding, LoginPorts } from './login.ts';
import { hyperlinkTarget, printedLink } from './signInLink.ts';

const LINK = cn('rounded-sm text-accent underline underline-offset-2 hover:text-accent-hover', RING);

type OpenChannel = Exclude<LoginChannel, { readonly phase: 'idle' }>;

function browserLoginPorts(): LoginPorts {
  return { location: window.location, socket: browserSocket, signedIn: () => currentFloorSession() !== null, offer: socketOffer(floorSigner()) };
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
  replaced: 'Another sign-in for this engine took its place.',
  not_installed: 'Dudes could not find this engine on this computer.',
  unavailable: 'This engine cannot be signed in from the Floor on this computer.',
  could_not_start: 'The sign-in could not start.',
  needs_terminal: 'Signing in to Pi needs Attach terminals turned on under Floor switches in Settings.',
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

export function EngineSignIn({ engine, row, terminal, onSettings }: {
  readonly engine: HireEngine;
  readonly row: ProviderRow | null;
  readonly terminal: boolean;
  readonly onSettings?: () => void;
}) {
  const channel = useSyncExternalStore(subscribeToLogin, getLoginState, getLoginState);
  return <EngineSignInView engine={engine} row={row} channel={channel} terminal={terminal} onSettings={onSettings} />;
}

export function EngineSignInView({ engine, row, channel, terminal, onSettings }: {
  readonly engine: HireEngine;
  readonly row: ProviderRow | null;
  readonly channel: LoginChannel;
  /** Whether the Floor's terminal is on, which Pi's sign-in needs. */
  readonly terminal: boolean;
  readonly onSettings?: () => void;
}) {
  const name = HIRE_ENGINES[engine];
  if (channel.phase !== 'idle' && channel.engine === engine) {
    return <LoginPanel key={channel.id} engine={engine} channel={channel} />;
  }
  if (!signInOffered(engine, row)) return null;
  if (engine === 'pi' && !terminal) {
    return (
      <div className="mt-2 flex flex-col items-start gap-2" data-login-offer="pi">
        <Notice tone="warn" role="status">{ENDINGS.needs_terminal}{' '}
          <a href="#/settings" onClick={onSettings} className={LINK}>Open Settings</a></Notice>
        <Button data-login="recheck" onClick={() => refreshFloorSnapshot()}>Check again</Button>
      </div>
    );
  }
  const busy = channel.phase !== 'idle' && channel.phase !== 'ended';
  return (
    <div className="mt-2 flex flex-col items-start gap-2" data-login-offer={engine}>
      {engine === 'pi' ? <p className="m-0 text-small text-muted">
        Pi cannot tell Dudes whether it is signed in. If it needs a sign-in, open it here.
      </p> : null}
      <Button variant="primary" data-login="start" aria-disabled={busy || undefined}
        title={busy ? 'Finish or cancel the other sign-in first.' : undefined}
        onClick={() => { if (!busy) startLogin(engine, browserLoginPorts()); }}>
        {engine === 'pi' ? 'Open Pi to sign in' : `Sign in to ${name}`}
      </Button>
    </div>
  );
}

function LoginPanel({ engine, channel }: { readonly engine: HireEngine; readonly channel: OpenChannel }) {
  const ended = channel.phase === 'ended';
  const failed = ended && channel.ending !== 'succeeded';
  const progress = channel.phase === 'opening' || channel.phase === 'starting' ? PROGRESS[channel.phase] : null;
  const [link, setLink] = useState<string | null>(null);
  return (
    <section className="mt-2 flex flex-col gap-3 rounded-lg border border-line bg-surface-2 p-3 font-sans" aria-label={`Sign in to ${HIRE_ENGINES[engine]}`}
      data-login-phase={channel.phase} data-login-ending={ended ? channel.ending : undefined}>
      <Notice tone={failed ? 'danger' : 'info'} data-login="state" role={failed ? 'alert' : 'status'}>
        {ended ? endingText(engine, channel.ending) : progress ?? STEPS[engine]}
      </Notice>
      {link !== null && !ended ? <SignInLink link={link} /> : null}
      <LoginScreen phase={channel.phase} onLink={setLink} />
      <div className="flex flex-wrap gap-2">
        {failed ? <Button variant="primary" data-login="retry"
          onClick={() => startLogin(engine, browserLoginPorts())}>Try again</Button> : null}
        <Button data-login="close" onClick={closeLogin}>{ended ? 'Close' : 'Cancel sign-in'}</Button>
      </div>
    </section>
  );
}

/** The link the engine printed, shown whole so it can be read and selected, with a way to open or copy it. */
function SignInLink({ link }: { readonly link: string }) {
  const field = useRef<HTMLInputElement>(null);
  const [copy, setCopy] = useState<'idle' | 'copied' | 'select'>('idle');
  const copyLink = () => {
    const clipboard = navigator.clipboard;
    const refused = () => {
      field.current?.select();
      setCopy('select');
    };
    if (clipboard === undefined) refused();
    else clipboard.writeText(link).then(() => setCopy('copied'), refused);
  };
  return (
    <div className="flex flex-wrap items-center gap-2" data-login="link">
      <Input ref={field} className="min-w-56 flex-1 font-mono text-small" type="text" readOnly value={link} spellCheck={false}
        aria-label="Sign-in link" />
      <a data-login="open" className={buttonClass('secondary', 'md', false)} href={link} target="_blank" rel="noopener noreferrer">Open link</a>
      <Button data-login="copy" onClick={copyLink}>Copy link</Button>
      <span className="basis-full text-small text-muted empty:hidden" role="status">
        {copy === 'copied' ? 'Copied.' : copy === 'select' ? 'The browser blocked copying. The link is selected: press Ctrl+C or Cmd+C.' : ''}
      </span>
    </div>
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
function LoginScreen({ phase, onLink }: { readonly phase: OpenChannel['phase']; readonly onLink: (link: string | null) => void }) {
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
      // The link the engine wrote, as a terminal hyperlink when it sent one and otherwise as the rows it drew.
      let hyperlink: string | null = null;
      let scan = 0;
      const rescan = () => {
        scan = 0;
        const buffer = term.buffer.active;
        const rows = [];
        for (let y = 0; y < buffer.length; y += 1) {
          const row = buffer.getLine(y);
          if (row !== undefined) rows.push({ text: row.translateToString(true), wrapped: row.isWrapped });
        }
        onLink(printedLink(rows, term.cols) ?? hyperlink);
      };
      const hyperlinks = term.parser.registerOscHandler(8, (data) => {
        hyperlink = hyperlinkTarget(data) ?? hyperlink;
        return false;
      });
      const unbind = bindLoginScreen({
        size: () => ({ cols: term.cols, rows: term.rows }),
        write: (bytes) => {
          term.write(bytes, () => {
            if (scan === 0) scan = window.setTimeout(rescan, 150);
          });
        },
      });
      const observer = new ResizeObserver(() => fit(term, element));
      observer.observe(element);
      screen.current = term;
      term.options.disableStdin = getLoginState().phase !== 'running';
      const retheme = () => { term.options.theme = themeOf(element); };
      // The shell names the scheme it draws in, which follows the browser's own when the theme is left on system.
      const shell = element.closest('.shell');
      const themes = new MutationObserver(retheme);
      if (shell !== null) themes.observe(shell, { attributes: true, attributeFilter: ['data-scheme'] });
      release = () => {
        window.clearTimeout(scan);
        hyperlinks.dispose();
        themes.disconnect();
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

  return <div ref={host} data-login="screen" className="terminal-screen h-52 flex-none overflow-hidden rounded-md border border-line" />;
}
