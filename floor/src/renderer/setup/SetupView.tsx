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
import { engineName } from '../engines.ts';
import { INSTALL_COMMANDS } from './daemon.ts';
import { ReadNotice, snapshotReadState } from '../ReadError.tsx';
import type { ProviderName, ProviderRow } from './daemon.ts';
import { Button, Disclosure, ICON } from '../../ui/index.ts';
import { Code, HINT, LINK, Tag } from '../PageParts.tsx';
import { Copy, ExternalLink, RefreshCw } from 'lucide-react';

/** Put `text` on the clipboard. Nothing else happens, and a refusal changes nothing. */
export function copyToClipboard(text: string): void {
  void navigator.clipboard?.writeText(text).catch(() => {});
}

/** The provider's own install guide, where this page knows one. */
function InstallGuide({ provider }: { readonly provider: ProviderName }) {
  switch (provider) {
    case 'claude':
      return (
        <a className={`${LINK} inline-flex items-center gap-1`} href="https://code.claude.com/docs/en/setup" target="_blank" rel="noopener noreferrer">
          Claude Code install guide<ExternalLink {...ICON} size={12} aria-hidden="true" />
        </a>
      );
    case 'codex':
      return (
        <a className={`${LINK} inline-flex items-center gap-1`} href="https://github.com/openai/codex" target="_blank" rel="noopener noreferrer">
          Codex CLI install guide<ExternalLink {...ICON} size={12} aria-hidden="true" />
        </a>
      );
    default:
      return null;
  }
}

function ProviderEntry({ row }: { readonly row: ProviderRow }) {
  const install = row.detail === null ? INSTALL_COMMANDS[row.provider] : undefined;
  return (
    <li data-provider={row.provider} data-found={row.found} data-ui="card"
      className="grid gap-2 rounded-lg border border-line bg-surface p-4 shadow-card">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="m-0 text-title font-semibold text-ink">{engineName(row.provider)}</h3>
        <Tag tone={row.found ? 'ok' : 'warn'}>{row.found ? 'Found' : 'Not found'}</Tag>
      </div>
      {row.found ? (
        <>
          <Disclosure summary="Where it is">
            <p className={HINT}>Dudes found it at <Code>{row.path ?? 'a path this page cannot show'}</Code>.</p>
          </Disclosure>
        </>
      ) : (
        <>
          {row.detail === null ? null : <p className="m-0 text-base leading-reading text-ink">{row.detail}</p>}
          <Disclosure summary="Why it was not found">
            <div className="grid gap-2">
              {row.detail === null ? <p className={HINT}>Dudes could not find <Code>{row.program ?? 'its program'}</Code>.</p> : null}
              <p className={HINT}>
                Dudes searches the same folders it had when it started. These may differ from the folders your terminal
                searches. If you install an app in one of them, press Recheck. If you change the search folders, restart Dudes.
              </p>
            </div>
          </Disclosure>
          {install === undefined ? null : (
            <div data-part="install" className="flex flex-wrap items-center gap-2 rounded-md bg-surface-2 p-2">
              <Code className="min-w-0 flex-1 bg-transparent">{install}</Code>
              <Button size="sm" icon={<Copy {...ICON} />} onClick={() => copyToClipboard(install)}>Copy</Button>
            </div>
          )}
          <div className="flex"><InstallGuide provider={row.provider} /></div>
        </>
      )}
    </li>
  );
}

export function SetupView({ floor, onGuide }: { readonly floor: FloorState; readonly onGuide?: () => void }) {
  const report = floor.daemon;
  const state = snapshotReadState(floor, report !== null);
  return (
    <div data-ui="setup" className="grid gap-4 font-sans text-ink [overflow-wrap:anywhere]">
      {onGuide === undefined ? null : <div className="flex"><Button id="first-run-guide-open" onClick={onGuide}>Open first-run guide</Button></div>}
      <ReadNotice state={state} label="Setup" message="Dudes could not check the engines. Retry to check again." />
      {report === null || state !== 'ready' ? null : (
          <>
            {floor.stale ? <p role="status" className={HINT}>Connection interrupted. These engine checks may be out of date.</p> : null}
            <Disclosure summary="Where Dudes keeps its data">
              <dl className="m-0 grid gap-0.5">
                <dt className="text-small font-medium tracking-label text-muted uppercase">State directory</dt>
                <dd className="m-0 text-base">{report.stateDir === null ? 'Not reported' : <Code>{report.stateDir}</Code>}</dd>
              </dl>
            </Disclosure>
            {report.providers.length === 0 ? <p className={HINT}>No engines are available to hire.</p> : (
              <ul className="m-0 grid list-none gap-3 p-0" aria-label="Engines">
                {report.providers.map((row) => <ProviderEntry key={row.provider} row={row} />)}
              </ul>
            )}
          </>
        )}
      {state !== 'ready' ? null : <div className="flex"><Button variant="primary" icon={<RefreshCw {...ICON} />} onClick={() => refreshFloorSnapshot()}>Recheck</Button></div>}
    </div>
  );
}
