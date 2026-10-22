// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Setup view: whether the daemon finds each provider's program on its own
// PATH, and where its state lives, as the latest snapshot reports them.
//
// Read-only. A provider the daemon cannot find shows its install command as
// text, a Copy button that writes that text to the clipboard, and a link to
// the provider's install guide. The operator runs the command; the page runs
// nothing and changes nothing. Recheck takes a fresh snapshot, which carries a
// fresh check.

import { refreshFloorSnapshot } from '../floor/stream.ts';
import type { FloorState } from '../floor/store.ts';
import { INSTALL_COMMANDS } from './daemon.ts';
import { ReadNotice, snapshotReadState } from '../ReadError.tsx';
import type { ProviderName, ProviderRow } from './daemon.ts';

const NAMES: Readonly<Record<ProviderName, string>> = Object.freeze({
  claude: 'Claude Code',
  codex: 'Codex CLI',
  copilot: 'GitHub Copilot CLI',
  antigravity: 'Antigravity',
  grok: 'Grok CLI',
  kimi: 'Kimi CLI',
  qwen: 'Qwen Code',
  opencode: 'opencode',
  pi: 'Pi',
  crush: 'Crush',
});

/** Put `text` on the clipboard. Nothing else happens, and a refusal changes nothing. */
export function copyToClipboard(text: string): void {
  void navigator.clipboard?.writeText(text).catch(() => {});
}

/** The provider's own install guide, where this page knows one. */
function InstallGuide({ provider }: { readonly provider: ProviderName }) {
  switch (provider) {
    case 'claude':
      return (
        <a href="https://code.claude.com/docs/en/setup" target="_blank" rel="noopener noreferrer">
          Claude Code install guide
        </a>
      );
    case 'codex':
      return (
        <a href="https://github.com/openai/codex" target="_blank" rel="noopener noreferrer">
          Codex CLI install guide
        </a>
      );
    default:
      return null;
  }
}

function ProviderEntry({ row }: { readonly row: ProviderRow }) {
  const install = row.detail === null ? INSTALL_COMMANDS[row.provider] : undefined;
  return (
    <li className="setup-provider" data-provider={row.provider} data-found={row.found}>
      <h3>{NAMES[row.provider]}</h3>
      {row.found ? (
        <>
          <p>Found.</p>
          <details className="setup-where"><summary>Where it is</summary>
            <p>Found on the daemon’s PATH at <code>{row.path ?? 'a path this page cannot show'}</code>.</p>
          </details>
        </>
      ) : (
        <>
          {row.detail === null ? <p>Not found.</p> : <p className="setup-refusal">{row.detail}</p>}
          <details className="setup-where"><summary>Why it was not found</summary>
            {row.detail === null ? <p><code>{row.program ?? 'Its program'}</code> is not on the daemon’s PATH.</p> : null}
            <p className="setup-note">
              The daemon searches the PATH it started with, which can differ from your shell’s. A program installed
              into a directory already on that PATH shows on Recheck; a changed PATH needs a daemon restart.
            </p>
          </details>
          {install === undefined ? null : (
            <p className="setup-install">
              <code>{install}</code>
              <button type="button" className="setup-copy" onClick={() => copyToClipboard(install)}>Copy</button>
            </p>
          )}
          <InstallGuide provider={row.provider} />
        </>
      )}
    </li>
  );
}

export function SetupView({ floor, onGuide }: { readonly floor: FloorState; readonly onGuide?: () => void }) {
  const report = floor.daemon;
  const state = snapshotReadState(floor, report !== null);
  return (
    <div className="setup-view">
      {onGuide === undefined ? null : <button type="button" id="first-run-guide-open" onClick={onGuide}>Open first-run guide</button>}
      <ReadNotice state={state} label="Setup" message="Not reported: the daemon’s provider check did not answer in time, or this page could not read it." />
      {report === null || state !== 'ready' ? null : (
          <>
            {floor.stale ? <p role="status">Setup unconfirmed. These values are from the last snapshot.</p> : null}
            <details className="setup-where"><summary>Where Dudes keeps its data</summary>
              <dl className="setup-home">
                <div>
                  <dt>State directory</dt>
                  <dd>{report.stateDir === null ? 'Not reported' : <code>{report.stateDir}</code>}</dd>
                </div>
              </dl>
            </details>
            {report.providers.length === 0 ? <p>The daemon has no provider it can hire.</p> : (
              <ul className="setup-providers" aria-label="Providers">
                {report.providers.map((row) => <ProviderEntry key={row.provider} row={row} />)}
              </ul>
            )}
          </>
        )}
      {state !== 'ready' ? null : <button type="button" className="setup-recheck" onClick={() => refreshFloorSnapshot()}>Recheck</button>}
    </div>
  );
}
