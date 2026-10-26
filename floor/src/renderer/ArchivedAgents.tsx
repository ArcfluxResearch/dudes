// SPDX-License-Identifier: MIT OR Apache-2.0

import { Button, Disclosure } from '../ui/index.ts';
import { HINT } from './PageParts.tsx';
import { RelativeTime } from './RelativeTime.tsx';
import { engineName } from './engines.ts';
import { useState, useSyncExternalStore } from 'react';
import type { ReactNode } from 'react';

import type { RetiredAgentRow } from './archive.ts';
import { accessoryOf, lookOf } from './appearance.ts';
import { PORTRAIT_FRAME, Portrait } from './Portrait.tsx';
import { worktreeText } from './worktree.ts';
import { ReadNotice, snapshotReadState } from './ReadError.tsx';
import { getReadAvailability, subscribeToReadAvailability } from './floor/reads.ts';
import type { FloorState } from './floor/store.ts';
import { formatCost } from './scene/label.ts';
import { hireSwitchOn } from './onboarding/HireEntry.tsx';
import { MemoryPanel } from './memory/MemoryPanel.tsx';
import { readHiddenArchiveIds, visibleArchivedAgents, writeHiddenArchiveIds } from './view.ts';

export function ArchivedAgents({ floor, onHireAgain }: {
  readonly floor: FloorState;
  readonly onHireAgain?: (row: RetiredAgentRow, opener: HTMLButtonElement) => void;
}) {
  const reads = useSyncExternalStore(subscribeToReadAvailability, getReadAvailability, getReadAvailability);
  const state = snapshotReadState(floor, reads.epoch !== floor.epoch || reads.archived);
  const [hidden, setHidden] = useState(readHiddenArchiveIds);
  const [showHidden, setShowHidden] = useState(false);
  const [saved, setSaved] = useState(true);
  const rows = visibleArchivedAgents(floor.retiredAgents, hidden, showHidden);
  const hiddenCount = floor.retiredAgents.filter((row) => hidden.includes(row.id)).length;
  const ready = floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch;
  function hide(id: string, value: boolean): void {
    const next = value ? [...hidden.filter((entry) => entry !== id), id].slice(-1024) : hidden.filter((entry) => entry !== id);
    setHidden(next);
    setSaved(writeHiddenArchiveIds(next));
  }
  return (
    <Disclosure className="archived-agents" summary={`Archived (${floor.retiredAgents.length})`}>
      <div className="grid gap-3 pt-1">
        <ReadNotice state={state} label="Archived agents" message="The archive was not reported, or this page could not read every archived agent. Retry to load the latest archive." />
        {state !== 'ready' || !floor.stale ? null : <p role="status" className={HINT}>Connection interrupted. This archive may be out of date.</p>}
        <div className="grid gap-1 text-small leading-reading text-muted">
          <p className="m-0">Recent retired agents. Up to 64 entries within the archive size limit.</p>
          <p className="m-0">Hidden entries stay in the archive. This view preference lives only in this browser.</p>
          <p className="m-0">Reported spend includes reported dollars and tokens. Usage without dollar reports is not priced. Unreported spend is not estimated.</p>
        </div>
        {hiddenCount === 0 ? null : <div className="flex"><Button size="sm" aria-pressed={showHidden}
          onClick={() => setShowHidden((value) => !value)}>{showHidden ? 'Hide hidden' : `Show hidden (${hiddenCount})`}</Button></div>}
        {saved ? null : <p role="status" className={HINT}>Browser storage is unavailable. This preference lasts until this view closes.</p>}
        {rows.length === 0 ? state === 'ready' ? <p className={HINT}>{floor.retiredAgents.length === 0 ? 'No archived agents.' : 'All archived entries are hidden in this browser.'}</p> : null : (
          <ol aria-label="Archived agents" className="m-0 grid list-none gap-3 p-0">
            {[...rows].reverse().map((row) => (
              <li key={row.id} data-archived-agent-id={row.id} className="grid gap-3 rounded-lg border border-line bg-surface p-3 shadow-card">
                <div className="flex items-start gap-3">
                  <ArchiveLook row={row} />
                  <div className="grid min-w-0 flex-1 gap-0.5">
                    <h3 className="m-0 text-body font-semibold text-ink [overflow-wrap:anywhere]">{row.name}</h3>
                    <p className="m-0 text-base text-muted">{row.role} · {engineName(row.provider)}</p>
                    <p className="m-0 text-small text-muted [overflow-wrap:anywhere]">{row.workspace_label ?? 'Project not recorded'}</p>
                    {row.worktree === undefined ? null : <p className="m-0 text-small text-muted">{worktreeText(row.worktree)}</p>}
                  </div>
                </div>
                <dl className="m-0 grid grid-cols-2 gap-x-4 gap-y-2 min-[641px]:grid-cols-4">
                  <ArchiveFact label="Hired"><RelativeTime at={row.hired_at} /></ArchiveFact>
                  <ArchiveFact label="Retired"><RelativeTime at={row.retired_at} /></ArchiveFact>
                  <ArchiveFact label="Work attempts">{row.turns}</ArchiveFact>
                  <ArchiveFact label="Reported spend">{formatCost(row.spend)}</ArchiveFact>
                </dl>
                <div className="flex flex-wrap gap-2">
                  {onHireAgain === undefined ? null : <Button size="sm" disabled={!ready || !hireSwitchOn(floor)}
                    title={!ready ? 'Reconnect to Dudes before hiring again.' : !hireSwitchOn(floor) ? 'Turn on Hire agents in Settings.' : undefined}
                    onClick={(event) => onHireAgain(row, event.currentTarget)}>Hire again</Button>}
                  <Button size="sm" variant="ghost" onClick={() => hide(row.id, !hidden.includes(row.id))}>
                    {hidden.includes(row.id) ? 'Show in archive' : 'Hide in this browser'}
                  </Button>
                </div>
                <ArchiveMemory agent={row.id} floor={floor} />
              </li>
            ))}
          </ol>
        )}
      </div>
    </Disclosure>
  );
}

function ArchiveFact({ label, children }: { readonly label: string; readonly children: ReactNode }) {
  return <div className="grid gap-0.5"><dt className="text-caption font-medium tracking-label text-muted uppercase">{label}</dt>
    <dd className="m-0 text-base text-ink">{children}</dd></div>;
}

/** A retired agent's memory, read only while its disclosure is open, so closing it forgets what was read. */
function ArchiveMemory({ agent, floor }: { readonly agent: string; readonly floor: FloorState }) {
  const [open, setOpen] = useState(false);
  return <Disclosure className="archive-memory border-t border-line pt-2" summary="Memory" onToggle={(event) => setOpen(event.currentTarget.open)}>
    {open ? <MemoryPanel agent={agent} active crew={floor} /> : null}
  </Disclosure>;
}

function ArchiveLook({ row }: { readonly row: RetiredAgentRow }) {
  const character = row.character === undefined ? 'Automatic character' : `Character ${row.character.toUpperCase()}`;
  const accent = row.accent === undefined ? 'Automatic accent' : `${row.accent[0]?.toUpperCase()}${row.accent.slice(1)} accent`;
  return <span data-character={row.character ?? 'automatic'} data-accent={row.accent ?? 'automatic'}
    title={`${character} · ${accent}`} className="inline-flex shrink-0 overflow-hidden rounded-md border border-line bg-surface-2">
    <Portrait className={`agent-portrait ${PORTRAIT_FRAME}`} look={lookOf(row)} accessory={accessoryOf(row.role)} name={row.name} />
    <span className="sr-only">{character}{' · '}{accent}</span>
  </span>;
}
