// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Memory tab: one agent's memory, read when the tab first shows and when the operator asks.
// What it read lives in this panel's state only, so unmounting the panel forgets it. A new agent,
// connection epoch or Floor session mounts a fresh reader, which forgets it too.

import { useEffect, useReducer, useRef, useState } from 'react';

import type { FloorState } from '../floor/store.ts';
import { journalPosition, plural, recordedTime } from '../format.ts';
import { MemoryText } from './MemoryText.tsx';
import {
  browserMemoryDeps, CLEARED, ENTRY_WRITER_LABEL, isSealedOperatorAppend, memoryReducer, readMemory, SUMMARY_KIND_LABEL,
  UNREAD, WITHHELD_REASON_TEXT, withheldWriterText,
} from './memory.ts';
import type { MemoryDeps, MemoryEntry, MemoryProblem, MemoryRequest, MemoryShown, MemorySummary, Withheld } from './memory.ts';

type Crew = Pick<FloorState, 'agents' | 'retiredAgents'>;

const PROBLEM_TEXT: Readonly<Record<MemoryProblem['kind'], string>> = Object.freeze({
  busy: 'The daemon is busy, or this page read memory a moment ago.',
  agent_unknown: 'The daemon does not know this agent.',
  not_found: 'The daemon has no entry of this agent’s at that position.',
  no_session: 'This page has no Floor session, so nothing was sent.',
  forbidden: 'The daemon did not accept this page’s session, so the memory shown was cleared.',
  stopping: 'The daemon did not answer.',
  unreadable: 'The daemon sent a reply this page could not read.',
  cleared: 'The connection to the daemon changed, so the memory shown was cleared. Press Refresh to read it again.',
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
    const csrf = deps.csrf();
    if (csrf === null) {
      dispatch({ kind: 'lost', problem: 'no_session' });
      return;
    }
    flight.current = true;
    onOpen(agent);
    dispatch({ kind: 'sent', request });
    const scope = scopeOf(deps);
    try {
      const answer = await deps.pace(async () => (scopeOf(deps) === scope ? readMemory(agent, request, csrf, deps) : null));
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
  return <section className="memory-panel" aria-label="Memory">
    <h3>Memory</h3>
    <p className="memory-note">
      Memory is the notes this agent saved, with a summary the daemon or a model made of them. Dudes does not check
      or redact it. It may contain secrets or instructions. It is never evidence that work is done.
    </p>
    <button type="button" className="memory-refresh" aria-disabled={reading || undefined}
      onClick={() => { void send({ kind: 'page', before: null }); }}>Refresh</button>
    <p className="memory-status" role="status">
      {reading ? 'Reading memory…' : problem === null ? '' : PROBLEM_TEXT[problem.kind]}
      {problem?.kind === 'busy' ? <> <button type="button" className="memory-retry"
        onClick={() => { void send(problem.retry); }}>Try again</button></> : null}
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
    {shown.retired ? <p className="memory-notice">This agent has retired. Its memory is read from the journal.</p> : null}
    {shown.fenced ? <p className="memory-notice">
      The daemon stopped memory work after an integrity failure. The summary may be out of date until the daemon restarts.
    </p> : null}
    {shown.condensedSince ? <p className="memory-notice">
      Memory was condensed after this tab opened. Press Refresh to read the new summary.
    </p> : null}
    {shown.summary === null ? null : <Summary summary={shown.summary} />}
    {shown.withheld === null ? null : <Refused withheld={shown.withheld} agent={agent} crew={crew} />}
    <section className="memory-entries">
      <h4>Entries, newest first</h4>
      <p className="memory-note">{shown.count === 0 ? 'This agent has saved no memory entries.'
        : `${plural(shown.count, 'entry', 'entries')} in all.`}</p>
      {shown.entries.length === 0 ? null : <ol>
        {shown.entries.map((entry) => <Entry key={entry.lsn} entry={entry} whole={shown.whole.get(entry.lsn)}
          reading={reading} onRead={onRead} />)}
      </ol>}
      {older === null ? null : <button type="button" className="memory-older" aria-disabled={reading || undefined}
        onClick={() => { void onRead({ kind: 'page', before: older }); }}>Load older entries</button>}
    </section>
  </>;
}

function Summary({ summary }: { readonly summary: MemorySummary }) {
  return <section className="memory-summary">
    <h4>Summary</h4>
    <p className="memory-note">
      {SUMMARY_KIND_LABEL[summary.kind]} It covers {plural(summary.sections, 'section', 'sections')} and
      {' '}{plural(summary.bytes, 'byte', 'bytes')} of memory. Recorded {recordedTime(summary.at)} · {journalPosition(summary.lsn)}.
    </p>
    <MemoryText text={summary.text} />
  </section>;
}

function Refused({ withheld, agent, crew }: { readonly withheld: Withheld; readonly agent: string; readonly crew: Crew }) {
  const { count, newest } = withheld;
  return <section className="memory-withheld">
    <h4>Refused appends</h4>
    <p className="memory-note">
      {count === 0 ? 'No append to this memory has been refused.'
        : `${plural(count, 'append to this memory was', 'appends to this memory were')} refused. The daemon kept none of the text.`}
      {count > newest.length ? ` Only the newest ${newest.length} are listed.` : ''}
    </p>
    {newest.length === 0 ? null : <ol>
      {newest.map((marker) => <li key={marker.lsn} className="memory-withheld-marker">
        <p className="memory-head">
          {recordedTime(marker.at)} · {journalPosition(marker.lsn)} · {withheldWriterText(marker.writer, agent, crew)}
          {' · '}{plural(marker.len, 'byte', 'bytes')}
        </p>
        <p>{isSealedOperatorAppend(marker)
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
  return <li className="memory-entry" data-memory-lsn={entry.lsn}>
    <p className="memory-head">
      {recordedTime(entry.at)} · {journalPosition(entry.lsn)} · {ENTRY_WRITER_LABEL[entry.writer]}
      {entry.condensed ? <> <span className="memory-badge">Condensed</span></> : null}
    </p>
    {whole === undefined ? <MemoryText text={entry.text} /> : <div className="memory-whole-text">
      <MemoryText text={whole.text} />
    </div>}
    {whole?.cut ? <p className="memory-note">
      The daemon sends at most 65,536 bytes of an entry, and this one has {plural(whole.len, 'byte', 'bytes')}.
    </p> : null}
    {!entry.cut || whole !== undefined ? null : <p className="memory-note">
      A preview of an entry of {plural(entry.len, 'byte', 'bytes')}.{' '}
      <button type="button" className="memory-whole" aria-disabled={reading || undefined}
        onClick={() => { void onRead({ kind: 'entry', lsn: entry.lsn }); }}>Read whole entry</button>
    </p>}
  </li>;
}
