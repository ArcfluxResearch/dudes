// SPDX-License-Identifier: MIT OR Apache-2.0

import { RelativeTime } from './RelativeTime.tsx';
import { ACTIVITY_LABELS, activityText } from './activity.ts';
import { useEffect, useState, useSyncExternalStore } from 'react';
import { filterActivity } from './activityFilters.ts';
import { readActivityFilters, writeActivityFilters } from './view.ts';
import { ReadNotice, snapshotReadState } from './ReadError.tsx';
import { getReadAvailability, subscribeToReadAvailability } from './floor/reads.ts';
import { journalPosition, recordedTime } from './format.ts';
import type { FloorState } from './floor/store.ts';

export function LogPanel({ floor }: { readonly floor: FloorState }) {
  const [filters, setFilters] = useState(readActivityFilters);
  const reads = useSyncExternalStore(subscribeToReadAvailability, getReadAvailability, getReadAvailability);
  const state = snapshotReadState(floor, reads.epoch !== floor.epoch || reads.activity);
  const agents = new Map<string, string>();
  for (const row of floor.activity) {
    if ('agent' in row) agents.set(row.agent.id, row.agent.name || 'Unnamed agent');
  }
  if (filters.agentId !== '' && !agents.has(filters.agentId)) agents.set(filters.agentId, 'Unavailable agent');
  const kinds = new Set<string>(floor.activity.map((row) => row.kind));
  if (filters.kind !== '') kinds.add(filters.kind);
  const rows = filterActivity([...floor.activity].reverse(), filters);

  useEffect(() => { writeActivityFilters(filters); }, [filters]);

  return (
    <section className="log-panel" aria-label="Recent activity">
      <ReadNotice state={state} label="Activity" message="Activity was not reported, or this page could not read every event. Retry to load the latest activity." />
      {state !== 'ready' ? null : <p role="status">{floor.stale ? 'Connection interrupted. This activity may be out of date.' : 'Recent activity, newest first.'}</p>}
      <div className="activity-filters" role="group" aria-label="Filter recent activity">
        <label>Agent
          <select value={filters.agentId} onChange={(event) => setFilters({ ...filters, agentId: event.currentTarget.value })}>
            <option value="">All agents</option>
            {[...agents].map(([id, name]) => <option value={id} key={id}>{name}</option>)}
          </select>
        </label>
        <label>Event
          <select value={filters.kind} onChange={(event) => setFilters({ ...filters, kind: event.currentTarget.value })}>
            <option value="">All events</option>
            {[...kinds].sort().map((kind) => <option value={kind} key={kind}>{ACTIVITY_LABELS[kind as keyof typeof ACTIVITY_LABELS] ?? 'Unavailable event'}</option>)}
          </select>
        </label>
        <label>Search
          <input type="search" value={filters.query} maxLength={128} placeholder="Search activity"
            onChange={(event) => setFilters({ ...filters, query: event.currentTarget.value })} />
        </label>
      </div>
      {rows.length === 0 ? state === 'ready' ? <p>{floor.activity.length === 0 ? 'No recent activity.' : 'No matching activity.'}</p> : null : (
        <ol aria-label="Recent events">
          {rows.map((row) => (
            <li key={row.lsn} data-activity-lsn={row.lsn}>
              <p>{activityText(row)}</p>
              <small><RelativeTime at={row.at} /></small>
              <details><summary>Details</summary><p>{recordedTime(row.at)}</p><p>Record position: {journalPosition(row.lsn)}</p>
                <p>Event: <code>{row.kind}</code></p>
                {'agent' in row ? <p>Agent: <code>{row.agent.id}</code></p> : null}
                {'card' in row ? <p>Ticket: <code>{row.card.id}</code></p> : null}
              </details>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}
