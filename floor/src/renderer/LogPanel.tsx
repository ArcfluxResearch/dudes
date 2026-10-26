// SPDX-License-Identifier: MIT OR Apache-2.0

import { Disclosure } from '../ui/Disclosure.tsx';
import { Field, SearchInput } from '../ui/Field.tsx';
import { Select } from '../ui/Select.tsx';
import { RelativeTime } from './RelativeTime.tsx';
import './LogPanel.css';
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

  const agentOptions = [{ value: '', label: 'All agents' }, ...[...agents].map(([value, label]) => ({ value, label }))];
  const eventOptions = [{ value: '', label: 'All events' }, ...[...kinds].sort().map((value) => ({
    value, label: ACTIVITY_LABELS[value as keyof typeof ACTIVITY_LABELS] ?? 'Unavailable event' }))];

  return (
    <section className="log-panel" data-ui-scope="" aria-label="Recent activity">
      <ReadNotice state={state} label="Activity" message="Activity was not reported, or this page could not read every event. Retry to load the latest activity." />
      {state !== 'ready' ? null : <p role="status" className="log-status">{floor.stale ? 'Connection interrupted. This activity may be out of date.' : 'Recent activity, newest first.'}</p>}
      <div className="activity-filters" role="group" aria-label="Filter recent activity">
        <Field label="Agent">
          <Select value={filters.agentId} options={agentOptions}
            onValueChange={(agentId) => setFilters({ ...filters, agentId })} />
        </Field>
        <Field label="Event">
          <Select value={filters.kind} options={eventOptions}
            onValueChange={(kind) => setFilters({ ...filters, kind })} />
        </Field>
        <Field label="Search">
          <SearchInput value={filters.query} maxLength={128} placeholder="Search activity"
            onChange={(event) => setFilters({ ...filters, query: event.currentTarget.value })} />
        </Field>
      </div>
      {rows.length === 0 ? state === 'ready' ? <p>{floor.activity.length === 0 ? 'No recent activity.' : 'No matching activity.'}</p> : null : (
        <ol aria-label="Recent events" className="log-rows">
          {rows.map((row) => (
            <li key={row.lsn} data-activity-lsn={row.lsn}>
              <p>{activityText(row)}</p>
              <small><RelativeTime at={row.at} /></small>
              <Disclosure summary="Details"><p>{recordedTime(row.at)}</p><p>Record position: {journalPosition(row.lsn)}</p>
                <p>Event: <code>{row.kind}</code></p>
                {'agent' in row ? <p>Agent: <code>{row.agent.id}</code></p> : null}
                {'card' in row ? <p>Ticket: <code>{row.card.id}</code></p> : null}
              </Disclosure>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}
