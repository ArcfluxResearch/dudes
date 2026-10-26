// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Memory tab: one agent's memory, read when the tab first shows and when the operator asks.
// What it read lives in this panel's state only, so unmounting the panel forgets it. A new agent,
// connection epoch or Floor session mounts a fresh reader, which forgets it too.

import { Button, Disclosure, Notice, cn } from '../../ui/index.ts';
import { RelativeTime } from '../RelativeTime.tsx';
import { useEffect, useReducer, useRef, useState } from 'react';
import type { ReactNode } from 'react';

import type { FloorState } from '../floor/store.ts';
import { journalPosition, plural, recordedTime } from '../format.ts';
import { HEADING, NOTE } from '../chat/look.ts';
import { MemoryText } from './MemoryText.tsx';
import {
  browserMemoryDeps, CLEARED, ENTRY_WRITER_LABEL, isSealedOperatorAppend, memoryReducer, readMemory, SUMMARY_KIND_LABEL,
  UNREAD, WITHHELD_REASON_TEXT, withheldWriterText,
} from './memory.ts';
import type { MemoryDeps, MemoryEntry, MemoryProblem, MemoryRequest, MemoryShown, MemorySummary, Withheld } from './memory.ts';

const BLOCK = 'flex flex-col gap-2';

type Crew = Pick<FloorState, 'agents' | 'retiredAgents'>;

const PROBLEM_TEXT: Readonly<Record<MemoryProblem['kind'], string>> = Object.freeze({
  busy: 'Dudes is busy, or this page read memory a moment ago.',
  agent_unknown: 'Dudes does not know this agent.',
  not_found: 'Dudes has no entry of this agent’s at that position.',
  no_session: 'This page has no Floor session, so nothing was sent.',
  forbidden: 'Dudes did not accept this page’s session, so the memory shown was cleared.',
  stopping: 'Dudes did not answer.',
  unreadable: 'Dudes sent a reply this page could not read.',
  cleared: 'The connection to Dudes changed, so the memory shown was cleared. Press Refresh to read it again.',
});

/** The epoch and session a reply must still belong to when it arrives. */
function scopeOf(deps: MemoryDeps): string {
  return `${deps.epoch()} ${deps.session()}`;
}

/**
 * `active` is whether the Memory tab is the one showing. Memory is read the first time it is,
 * and after that only when the operator asks; a reader remounted for a new epoch or session
 * waits for Refresh.
 */
export function MemoryPanel({ agent, active, crew, deps = browserMemoryDeps }: {
  readonly agent: string;
  readonly active: boolean;
  readonly crew: Crew;
  readonly deps?: MemoryDeps;
}) {
  const [opened, setOpened] = useState<string | null>(null);
  return <MemoryReader key={`${agent} ${scopeOf(deps)}`} agent={agent} active={active} crew={crew} deps={deps}
    reopened={opened === agent} onOpen={setOpened} />;
}

function MemoryReader({ agent, active, crew, deps, reopened, onOpen }: {
  readonly agent: string;
  readonly active: boolean;
  readonly crew: Crew;
  readonly deps: MemoryDeps;
  readonly reopened: boolean;
  readonly onOpen: (agent: string) => void;
}) {
  const [state, dispatch] = useReducer(memoryReducer, reopened ? CLEARED : UNREAD);
  const flight = useRef(false);

  async function send(request: MemoryRequest): Promise<void> {
    if (flight.current) return;
    if (!deps.signedIn()) {
      dispatch({ kind: 'lost', problem: 'no_session' });
      return;
    }
    flight.current = true;
    onOpen(agent);
    dispatch({ kind: 'sent', request });
    const scope = scopeOf(deps);
    try {
      const answer = await deps.pace(async () => (scopeOf(deps) === scope ? readMemory(agent, request, deps) : null));
      if (answer === null || scopeOf(deps) !== scope) {
        dispatch({ kind: 'lost', problem: 'cleared' });
        return;
      }
      dispatch(answer);
      if (answer.outcome.kind === 'forbidden') deps.refresh();
    } finally {
      flight.current = false;
    }
  }

  useEffect(() => {
    if (active && state.phase === 'unread') void send({ kind: 'page', before: null });
  }, [active, state.phase]);

  const reading = state.phase === 'reading';
  const problem = state.phase === 'read' ? state.problem : null;
  const shown = state.phase === 'unread' ? null : state.shown;
  return <section className="memory-panel flex flex-col gap-3 font-sans text-base text-ink" aria-label="Memory">
    <div className="flex items-center justify-between gap-3">
      <h3 className="m-0 text-title font-semibold">Memory</h3>
      <Button size="sm" className="memory-refresh" aria-disabled={reading || undefined}
        title={reading ? 'Wait for memory to finish loading.' : undefined}
        onClick={() => { void send({ kind: 'page', before: null }); }}>Refresh</Button>
    </div>
    <p className={cn('memory-note', NOTE)}>
      Memory is the notes this agent saved, with a summary Dudes or a model made of them. Dudes does not check
      or redact it. It may contain secrets or instructions. It is never evidence that work is done.
    </p>
    <p className="memory-status m-0 flex min-h-5 flex-wrap items-center gap-2 text-small text-muted empty:hidden" role="status">
      {reading ? 'Reading memory…' : problem === null ? '' : PROBLEM_TEXT[problem.kind]}
      {problem?.kind === 'busy' ? <>{' '}<Button size="sm" className="memory-retry"
        onClick={() => { void send(problem.retry); }}>Try again</Button></> : null}
    </p>
    {shown === null ? null : <Shown shown={shown} agent={agent} crew={crew} reading={reading} onRead={send} />}
  </section>;
}

function Shown({ shown, agent, crew, reading, onRead }: {
  readonly shown: MemoryShown;
  readonly agent: string;
  readonly crew: Crew;
  readonly reading: boolean;
  readonly onRead: (request: MemoryRequest) => Promise<void>;
}) {
  const older = shown.older;
  return <>
    {shown.retired ? <Notice className="memory-notice">This agent has retired. Its saved memory is still available.</Notice> : null}
    {shown.fenced ? <Notice tone="warn" className="memory-notice">
      Dudes stopped memory work after an integrity failure. The summary may be out of date until Dudes restarts.
    </Notice> : null}
    {shown.condensedSince ? <Notice className="memory-notice">
      Memory was condensed after this tab opened. Press Refresh to read the new summary.
    </Notice> : null}
    {shown.summary === null ? null : <Summary summary={shown.summary} />}
    {shown.withheld === null ? null : <Refused withheld={shown.withheld} agent={agent} crew={crew} />}
    <section className={cn('memory-entries', BLOCK)}>
      <h4 className={HEADING}>Entries, newest first</h4>
      <p className={cn('memory-note', NOTE)}>{shown.count === 0 ? 'This agent has saved no memory entries.'
        : `${plural(shown.count, 'entry', 'entries')} in all.`}</p>
      {shown.entries.length === 0 ? null : <ol className="m-0 list-none p-0">
        {shown.entries.map((entry) => <Entry key={entry.lsn} entry={entry} whole={shown.whole.get(entry.lsn)}
          reading={reading} onRead={onRead} />)}
      </ol>}
      {older === null ? null : <Button className="memory-older self-start" aria-disabled={reading || undefined}
        title={reading ? 'Wait for memory to finish loading.' : undefined}
        onClick={() => { void onRead({ kind: 'page', before: older }); }}>Load older entries</Button>}
    </section>
  </>;
}

function Summary({ summary }: { readonly summary: MemorySummary }) {
  return <section className={cn('memory-summary overflow-hidden rounded-lg border border-line bg-surface p-3', BLOCK)}>
    <h4 className={HEADING}>Summary</h4>
    <p className={cn('memory-note', NOTE)}>
      {SUMMARY_KIND_LABEL[summary.kind]} It covers {plural(summary.sections, 'section', 'sections')} and
      {' '}{plural(summary.bytes, 'byte', 'bytes')} of memory. Updated <RelativeTime at={summary.at} />.
    </p>
    <div className="memory-summary-text max-h-96 overflow-auto"><MemoryText text={summary.text} /></div>
    <Details>{recordedTime(summary.at)} · {journalPosition(summary.lsn)}</Details>
  </section>;
}

function Refused({ withheld, agent, crew }: { readonly withheld: Withheld; readonly agent: string; readonly crew: Crew }) {
  const { count, newest } = withheld;
  return <section className={cn('memory-withheld', BLOCK)}>
    <h4 className={HEADING}>Entries not saved</h4>
    <p className={cn('memory-note', NOTE)}>
      {count === 0 ? 'No memory entries were refused.'
        : `Dudes did not save ${plural(count, 'entry', 'entries')}. It kept none of the text.`}
      {count > newest.length ? ` Only the newest ${newest.length} are listed.` : ''}
    </p>
    {newest.length === 0 ? null : <ol className="m-0 list-none p-0">
      {newest.map((marker) => <li key={marker.lsn} className="memory-withheld-marker overflow-hidden border-t border-line py-2">
        <p className="memory-head m-0 mb-1 text-small text-muted">
          <RelativeTime at={marker.at} /> · {withheldWriterText(marker.writer, agent, crew)}
          {' · '}{plural(marker.len, 'byte', 'bytes')}
        </p>
        <Details>{recordedTime(marker.at)} · {journalPosition(marker.lsn)}</Details>
        <p className="m-0 mt-1 text-small">{isSealedOperatorAppend(marker)
          ? <>Refused: once an agent has been hired, only the agents themselves can add to memory. A note from outside every
            agent is withheld.</>
          : WITHHELD_REASON_TEXT[marker.reason]}</p>
      </li>)}
    </ol>}
  </section>;
}

function Entry({ entry, whole, reading, onRead }: {
  readonly entry: MemoryEntry;
  readonly whole: MemoryEntry | undefined;
  readonly reading: boolean;
  readonly onRead: (request: MemoryRequest) => Promise<void>;
}) {
  return <li className="memory-entry overflow-hidden border-t border-line py-2" data-memory-lsn={entry.lsn}>
    <p className="memory-head m-0 mb-1 text-small text-muted">
      <RelativeTime at={entry.at} /> · {ENTRY_WRITER_LABEL[entry.writer]}
      {entry.condensed ? <> <span className="memory-badge rounded-pill border border-line px-1.5 text-caption">Condensed</span></> : null}
    </p>
    <Details>{recordedTime(entry.at)} · {journalPosition(entry.lsn)}</Details>
    {whole === undefined ? <MemoryText text={entry.text} /> : <div className="memory-whole-text max-h-96 overflow-auto">
      <MemoryText text={whole.text} />
    </div>}
    {whole?.cut ? <p className={cn('memory-note', NOTE)}>
      Dudes sends at most 65,536 bytes of an entry, and this one has {plural(whole.len, 'byte', 'bytes')}.
    </p> : null}
    {!entry.cut || whole !== undefined ? null : <p className={cn('memory-note flex flex-wrap items-center gap-2', NOTE)}>
      A preview of an entry of {plural(entry.len, 'byte', 'bytes')}.{' '}
      <Button size="sm" className="memory-whole" aria-disabled={reading || undefined}
        title={reading ? 'Wait for memory to finish loading.' : undefined}
        onClick={() => { void onRead({ kind: 'entry', lsn: entry.lsn }); }}>Read whole entry</Button>
    </p>}
  </li>;
}

/** The record's exact time and position, closed until asked for. */
function Details({ children }: { readonly children: ReactNode }) {
  return <Disclosure className="text-small" summaryClassName="py-0 text-small font-normal text-muted" summary="Details">
    <p className="m-0 text-small text-muted">{children}</p>
  </Disclosure>;
}
