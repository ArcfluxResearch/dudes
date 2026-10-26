// SPDX-License-Identifier: MIT OR Apache-2.0

import { Disclosure, Field, Input, Select } from '../ui/index.ts';
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

  const agentOptions = [{ value: '', label: 'All agents' }, ...[...agents].map(([id, name]) => ({ value: id, label: name }))];
  const kindOptions = [{ value: '', label: 'All events' }, ...[...kinds].sort().map((kind) => ({
    value: kind, label: ACTIVITY_LABELS[kind as keyof typeof ACTIVITY_LABELS] ?? 'Unavailable event' }))];

  return (
    <section className="activity-log ui grid gap-4 font-sans text-ink" aria-label="Recent activity">
      <ReadNotice state={state} label="Activity" message="Activity was not reported, or this page could not read every event. Retry to load the latest activity." />
      {state !== 'ready' ? null : <p role="status" className="m-0 text-base text-muted">{floor.stale ? 'Connection interrupted. This activity may be out of date.' : 'Recent activity, newest first.'}</p>}
      <div className="activity-filters grid gap-3 min-[641px]:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_minmax(0,2fr)]" role="group" aria-label="Filter recent activity">
        <Field label="Agent">
          {(c) => <Select id={c.id} aria-describedby={c['aria-describedby']} aria-invalid={c['aria-invalid']} options={agentOptions} value={filters.agentId}
            onValueChange={(agentId) => setFilters({ ...filters, agentId })} />}
        </Field>
        <Field label="Event">
          {(c) => <Select id={c.id} aria-describedby={c['aria-describedby']} aria-invalid={c['aria-invalid']} options={kindOptions} value={filters.kind}
            onValueChange={(kind) => setFilters({ ...filters, kind })} />}
        </Field>
        <Field label="Search">
          {(c) => <Input id={c.id} aria-describedby={c['aria-describedby']} aria-invalid={c['aria-invalid']} type="search" value={filters.query} maxLength={128} placeholder="Search activity"
            onChange={(event) => setFilters({ ...filters, query: event.currentTarget.value })} />}
        </Field>
      </div>
      {rows.length === 0 ? state === 'ready' ? <p className="m-0 text-base text-muted">{floor.activity.length === 0 ? 'No recent activity.' : 'No matching activity.'}</p> : null : (
        <ol aria-label="Recent events" className="m-0 list-none divide-y divide-line overflow-hidden rounded-lg border border-line bg-surface p-0">
          {rows.map((row) => (
            <li key={row.lsn} data-activity-lsn={row.lsn} className="px-3 py-0.5 hover:bg-surface-2">
              <Disclosure summaryClassName="w-full" summary={<>
                <span className="min-w-0 flex-1 font-normal text-ink [overflow-wrap:anywhere]">{activityText(row)}</span>
                <span className="shrink-0 text-small font-normal tabular-nums text-muted"><RelativeTime at={row.at} /></span>
              </>}>
                <div className="grid gap-0.5 pb-2 text-small text-muted">
                  <p className="m-0">{recordedTime(row.at)}</p>
                  <p className="m-0">Record position: {journalPosition(row.lsn)}</p>
                  <p className="m-0">Event: <code className="font-mono text-small">{row.kind}</code></p>
                  {'agent' in row ? <p className="m-0">Agent: <code className="font-mono text-small">{row.agent.id}</code></p> : null}
                  {'card' in row ? <p className="m-0">Ticket: <code className="font-mono text-small">{row.card.id}</code></p> : null}
                </div>
              </Disclosure>
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}
