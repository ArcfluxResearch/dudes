// SPDX-License-Identifier: MIT OR Apache-2.0

import { useState, useSyncExternalStore } from 'react';

import type { RetiredAgentRow } from './archive.ts';
import { accessoryOf, lookOf } from './appearance.ts';
import { Portrait } from './Portrait.tsx';
import { recordedTime } from './format.ts';
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
    <details className="archived-agents">
      <summary>Archived ({floor.retiredAgents.length})</summary>
      <ReadNotice state={state} label="Archived agents" message="The archive was not reported, or this page could not read every archived agent. Retry to request a fresh snapshot." />
      {state !== 'ready' || !floor.stale ? null : <p role="status">Archive unconfirmed. These agents are from the last snapshot.</p>}
      <p>Recent retired agents. Up to 64 entries within the archive size limit.</p>
      <p>Hidden entries stay in the daemon's archive. This view preference lives only in this browser.</p>
      {hiddenCount === 0 ? null : <button type="button" aria-pressed={showHidden}
        onClick={() => setShowHidden((value) => !value)}>{showHidden ? 'Hide hidden' : `Show hidden (${hiddenCount})`}</button>}
      {saved ? null : <p role="status">Browser storage is unavailable. This preference lasts until this view closes.</p>}
      <p>Reported spend includes reported dollars and tokens. Usage without dollar reports is not priced. Unreported spend is not estimated.</p>
      {rows.length === 0 ? state === 'ready' ? <p>{floor.retiredAgents.length === 0 ? 'No archived agents.' : 'All archived entries are hidden in this browser.'}</p> : null : (
        <ol aria-label="Archived agents">
          {[...rows].reverse().map((row) => (
            <li key={row.id} data-archived-agent-id={row.id}>
              <h3>{row.name}</h3>
              <p>{row.role} · {row.provider}</p>
              <ArchiveLook row={row} />
              <p>{row.workspace_label ?? 'Project not recorded'}</p>
              {row.worktree === undefined ? null : <p>{worktreeText(row.worktree)}</p>}
              <dl>
                <div><dt>Hired</dt><dd>{recordedTime(row.hired_at)}</dd></div>
                <div><dt>Retired</dt><dd>{recordedTime(row.retired_at)}</dd></div>
                <div><dt>Total recorded turns</dt><dd>{row.turns}</dd></div>
                <div><dt>Reported spend</dt><dd>{formatCost(row.spend)}</dd></div>
              </dl>
              {onHireAgain === undefined ? null : <button type="button" disabled={!ready || !hireSwitchOn(floor)}
                onClick={(event) => onHireAgain(row, event.currentTarget)}>Hire again</button>}
              <button type="button" onClick={() => hide(row.id, !hidden.includes(row.id))}>
                {hidden.includes(row.id) ? 'Show in archive' : 'Hide in this browser'}
              </button>
              <ArchiveMemory agent={row.id} floor={floor} />
            </li>
          ))}
        </ol>
      )}
    </details>
  );
}

/** A retired agent's memory, read only while its disclosure is open, so closing it forgets what was read. */
function ArchiveMemory({ agent, floor }: { readonly agent: string; readonly floor: FloorState }) {
  const [open, setOpen] = useState(false);
  return <details className="archive-memory" onToggle={(event) => setOpen(event.currentTarget.open)}>
    <summary>Memory</summary>
    {open ? <MemoryPanel agent={agent} active crew={floor} /> : null}
  </details>;
}

function ArchiveLook({ row }: { readonly row: RetiredAgentRow }) {
  return <p className="archive-look" data-character={row.character ?? 'automatic'}
    data-accent={row.accent ?? 'automatic'}>
    <Portrait className="agent-portrait" look={lookOf(row)} accessory={accessoryOf(row.role)} name={row.name} />
    <span>{row.character === undefined ? 'Automatic character' : `Character ${row.character.toUpperCase()}`}
      {' · '}{row.accent === undefined ? 'Automatic accent' : `${row.accent[0]?.toUpperCase()}${row.accent.slice(1)} accent`}</span>
  </p>;
}
