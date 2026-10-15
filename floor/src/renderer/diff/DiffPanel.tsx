// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useId, useMemo, useRef, useState } from 'react';
import type { DiffContent, DiffEntry, DiffFreshness, DiffKind, DiffOmissionReason, DiffOutcome, DiffPage,
  DiffRefusalReason, DiffRequest, DiffTransport } from './diff.ts';
import { sameDiffBinding } from './diff.ts';
import './diff.css';

const BADGE: Readonly<Record<DiffFreshness, string>> = {
  matches_review_sample: 'Unchanged since the check',
  changed_after_check: 'Changed since the check',
  unknown: 'Unknown',
};
const FRESHNESS: Readonly<Record<DiffFreshness, string>> = {
  matches_review_sample: 'The files Dudes could read are the same as when it sampled them just after the check ran. '
    + 'This does not show that the check passed on these changes, and it does not cover the files left out below.',
  changed_after_check: 'Some files changed after the check ran, so these are not the files the check saw. '
    + 'Look at them again before you accept.',
  unknown: 'Dudes cannot tell whether this is what the check saw. It has no complete sample from just after the check, '
    + 'which is common: leaving out any one file makes a sample incomplete. Unknown does not mean fresh.',
};
const KINDS: Readonly<Record<DiffKind, string>> = {
  added: 'Added', modified: 'Modified', deleted: 'Deleted', renamed: 'Renamed', type_changed: 'Type changed',
};
const OMISSIONS: Readonly<Record<DiffOmissionReason, string>> = {
  binary: 'Binary file. Its contents are not shown.',
  unsupported_text: 'This file has characters the viewer cannot show safely.',
  sensitive_path: 'The name of this file suggests it holds secrets, so it is not shown.',
  sensitive_content: 'This file looks like it holds a secret, so it is not shown.',
  oversize_file: 'Too large to show here. Files over 16 KiB per side are not shown.',
  oversize_line: 'This file has a line longer than 4 KiB, so it is not shown.',
  unsupported_kind: 'This kind of file has no text to show.',
  raced: 'This file changed while it was being read, so it is not shown.',
  unreadable: 'This file could not be read.',
  response_limit: 'Showing this file would pass the size limit for one reply.',
};
const REFUSALS: Readonly<Record<DiffRefusalReason, string>> = {
  baseline_unavailable: 'Dudes did not record where this round started, so it cannot list the changes. '
    + 'Open the checkout in your editor with dudes ticket open.',
  stale_ticket: 'This ticket has moved on since the list was made. Choose Refresh to see its current changes.',
  stale_manifest: 'This list is out of date. Choose Refresh to make a new one.',
  expired: 'This list expired after a minute. Choose Refresh to make a new one.',
  invalid_request: 'The daemon could not use that request. Choose Refresh to start again.',
  disabled: 'The Changes view is turned off on this daemon.',
  busy: 'Dudes is reading a checkout for someone else, or you asked again too soon. Wait a few seconds, then choose Refresh.',
  session_limit: 'This Floor session has used all the reading it is allowed (about 1 MiB), and Refresh does not renew it. '
    + 'Open the checkout in your editor with dudes ticket open, or start a new Floor session with dudes floor open.',
  unsupported_platform: 'The Changes view does not work on this daemon’s operating system. Use dudes ticket open instead.',
  worktree_mismatch: 'The agent’s worktree no longer matches the one Dudes recorded, so nothing was read.',
  unsupported_storage: 'This repository uses storage the Changes view cannot read. Use dudes ticket open instead.',
  root_mismatch: 'The folder this ticket was recorded in has been replaced, so nothing was read.',
  timed_out: 'Reading the checkout took too long. Choose Refresh to try again.',
  git_unavailable: 'The daemon found no usable Git program to read the checkout with.',
  unreadable: 'The checkout could not be read.',
};
const ABSENT_SIDE: Readonly<Record<DiffKind, { readonly old: string; readonly current: string }>> = {
  added: { old: 'Not in the commit this round started from.', current: 'Empty.' },
  modified: { old: 'Empty.', current: 'Empty.' },
  deleted: { old: 'Empty.', current: 'Deleted: nothing is left in the checkout.' },
  renamed: { old: 'Empty.', current: 'Empty.' },
  type_changed: { old: 'Empty.', current: 'Empty.' },
};

export interface DiffReviewState {
  readonly page: DiffPage;
  readonly entries: readonly DiffEntry[];
  readonly opened: string | null;
  readonly content: DiffContent | null;
}
type Failure = DiffRefusalReason | 'forbidden' | 'unavailable' | 'unknown';
type PanelState = { readonly owner: { readonly key: string; readonly transport: DiffTransport } } & (
  | { readonly phase: 'closed' }
  | { readonly phase: 'loading'; readonly review: DiffReviewState | null }
  | { readonly phase: 'ready'; readonly review: DiffReviewState }
  | { readonly phase: 'refused'; readonly reason: Failure }
);

function failureText(reason: Failure): string {
  if (reason === 'forbidden') return 'This page has no current Floor session, so nothing was read. Reload the page to sign in again.';
  if (reason === 'unavailable') return 'The daemon did not answer. Choose Refresh to try again.';
  if (reason === 'unknown') return 'The daemon sent a reply this page could not read. Choose Refresh to try again.';
  return REFUSALS[reason];
}

function SourceLines({ label, text, absent }: {
  readonly label: 'Old' | 'Current';
  readonly text: string;
  readonly absent: string;
}) {
  const endsWithNewline = /[\r\n]$/.test(text);
  const lineCount = text === '' ? 0 : (text.match(/\r\n|\r|\n/g)?.length ?? 0) + (endsWithNewline ? 0 : 1);
  const numbers = Array.from({ length: lineCount }, (_, index) => index + 1).join('\n');
  return <section className="diff-side">
    <h6>{label}</h6>
    {text === '' ? <p className="diff-note">{absent}</p> : <>
      <div className="diff-source" tabIndex={0} role="region" aria-label={label === 'Old' ? 'Old source lines' : 'Current source lines'}>
        <pre><span className="diff-line-number" aria-hidden="true">{numbers}</span><code>{text}</code></pre>
      </div>
      {!endsWithNewline ? <p className="diff-note">No final newline.</p> : null}
    </>}
  </section>;
}

function Compare({ content, entry }: { readonly content: DiffContent; readonly entry: DiffEntry }) {
  const file = content.files.find((row) => row.entry_id === entry.entry_id);
  const omission = content.omitted.find((row) => row.entry_id === entry.entry_id);
  return <div className="diff-compare">
    {file === undefined ? null : <>
      {file.old_path === null ? null : <p className="diff-note">Renamed from <span className="diff-path">{file.old_path}</span></p>}
      <div className="diff-sides">
        <SourceLines label="Old" text={file.old} absent={ABSENT_SIDE[file.kind].old} />
        <SourceLines label="Current" text={file.current} absent={ABSENT_SIDE[file.kind].current} />
      </div>
    </>}
    {omission === undefined ? null : <p className="diff-omission">{OMISSIONS[omission.reason]}</p>}
    {content.truncated ? <p className="diff-omission">The reply reached its size limit, so the file is not shown.</p> : null}
  </div>;
}

export function DiffReview({ review, loading, onOpen, onPage }: {
  readonly review: DiffReviewState;
  readonly loading: boolean;
  readonly onOpen: (entry: DiffEntry) => void;
  readonly onPage: () => void;
}) {
  const id = useId();
  const { page, entries, opened, content } = review;
  return <>
    <p className="diff-fresh">
      <span className="diff-badge" data-freshness={page.freshness}>{BADGE[page.freshness]}</span>{' '}
      <span className="diff-fresh-line">{FRESHNESS[page.freshness]}</span>
    </p>
    {page.binding.preexisting_tracked ? <p className="diff-warning">
      Some tracked files already had changes when this round began, so the list below can include changes the agent did not make.</p> : null}
    <p className="diff-note">Round {page.binding.round}. Files Git does not track yet, files left out, and changes the bounded read cannot see
      are outside this comparison.</p>
    {page.omitted > 0 || page.truncated ? <p className="diff-omission">{page.omitted_lower_bound ? 'At least ' : ''}{page.omitted} changed {page.omitted === 1 ? 'file was' : 'files were'} left out of this list.{page.truncated ? ' The list reached its limit.' : ''}</p> : null}
    {page.untracked > 0 ? <p className="diff-note">{page.untracked_lower_bound ? 'At least ' : ''}{page.untracked} untracked {page.untracked === 1 ? 'file is' : 'files are'} not listed. Their names and contents are never shown.</p> : null}
    {entries.length === 0 ? <p>No tracked file differs from where this round began.</p> : <ul className="diff-selection">
      {entries.map((entry, index) => {
        const isOpen = entry.entry_id !== null && opened === entry.entry_id;
        return <li key={index}>
          <div className="diff-row-name"><span className="diff-kind">{KINDS[entry.kind]}</span>{' '}<span className="diff-path" id={`${id}-${index}`}>{entry.path}</span></div>
          {entry.availability.state === 'unavailable' || entry.entry_id === null
            ? <p className="diff-note">{entry.availability.state === 'unavailable' ? OMISSIONS[entry.availability.reason] : ''}</p>
            : <button type="button" aria-describedby={`${id}-${index}`} aria-pressed={isOpen} disabled={loading}
              onClick={() => { onOpen(entry); }}>{isOpen ? 'Close file' : 'Open side by side'}</button>}
          {isOpen && content !== null ? <Compare content={content} entry={entry} /> : null}
        </li>;
      })}
    </ul>}
    {page.next_cursor === null ? null : <div className="diff-actions">
      <button type="button" disabled={loading} onClick={onPage}>Show more files</button>
    </div>}
    <p className="diff-note">For omitted files or a bigger review, run <code>dudes ticket open {page.binding.card}</code> in your terminal.</p>
  </>;
}

function matchesReview(review: DiffReviewState, response: DiffPage | DiffContent): boolean {
  return response.manifest_id === review.page.manifest_id && response.expires_at === review.page.expires_at
    && sameDiffBinding(response.binding, review.page.binding);
}

function validContent(review: DiffReviewState, opened: string, content: DiffContent): boolean {
  const ids = [...content.files.map((file) => file.entry_id), ...content.omitted.map((row) => row.entry_id)];
  return ids.length === 1 && ids[0] === opened
    && content.files.every((file) => review.entries.some((entry) => entry.entry_id === file.entry_id
      && entry.path === file.path && entry.old_path === file.old_path && entry.kind === file.kind));
}

export function DiffPanel({ card, expectedSeq, transport }: {
  readonly card: string;
  readonly expectedSeq: number;
  readonly transport: DiffTransport;
}) {
  const key = `${card}:${expectedSeq}:${transport.generation()}`;
  const owner = useMemo(() => ({ key, transport }), [key, transport]);
  const currentOwner = useRef(owner);
  currentOwner.current = owner;
  const latest = useRef(0);
  const inflight = useRef<number | null>(null);
  const [state, setState] = useState<PanelState>({ owner, phase: 'closed' });
  useEffect(() => () => { latest.current += 1; inflight.current = null; }, [owner]);
  const mine = state.owner === owner;
  const closed = !mine || state.phase === 'closed';
  const loading = mine && state.phase === 'loading';
  const review = mine && (state.phase === 'ready' || state.phase === 'loading') ? state.review : null;

  async function read(request: DiffRequest, previous: DiffReviewState | null, opened: string | null = null): Promise<void> {
    if (inflight.current !== null) return;
    const ticket = ++latest.current;
    inflight.current = ticket;
    const generation = transport.generation();
    setState({ owner, phase: 'loading', review: previous });
    let outcome: DiffOutcome;
    try { outcome = await transport.request(request); } catch { outcome = { kind: 'unavailable' }; }
    if (inflight.current === ticket) inflight.current = null;
    if (ticket !== latest.current || currentOwner.current !== owner) return;
    if (generation !== transport.generation()) { setState({ owner, phase: 'closed' }); return; }
    if (outcome.kind !== 'response') { setState({ owner, phase: 'refused', reason: outcome.kind }); return; }
    const reply = outcome.response;
    if (reply.mode === 'refused') { setState({ owner, phase: 'refused', reason: reply.reason }); return; }
    if (reply.mode !== request.mode || reply.binding.card !== card || reply.binding.seq !== expectedSeq
      || (previous !== null && !matchesReview(previous, reply))) {
      setState({ owner, phase: 'refused', reason: 'unknown' }); return;
    }
    if (reply.mode === 'content') {
      if (previous === null || opened === null || !validContent(previous, opened, reply)) {
        setState({ owner, phase: 'refused', reason: 'unknown' }); return;
      }
      setState({ owner, phase: 'ready', review: { ...previous, opened, content: reply } }); return;
    }
    const entries = [...(previous?.entries ?? []), ...reply.entries];
    const ids = entries.flatMap((entry) => entry.entry_id === null ? [] : [entry.entry_id]);
    if (entries.length > 256 || new Set(ids).size !== ids.length) { setState({ owner, phase: 'refused', reason: 'unknown' }); return; }
    setState({ owner, phase: 'ready', review: { page: reply, entries, opened: previous?.opened ?? null, content: previous?.content ?? null } });
  }

  function open(entry: DiffEntry): void {
    if (state.phase !== 'ready' || !mine || entry.entry_id === null) return;
    if (state.review.opened === entry.entry_id) {
      setState({ owner, phase: 'ready', review: { ...state.review, opened: null, content: null } });
      return;
    }
    void read({ mode: 'content', manifest_id: state.review.page.manifest_id, entries: [entry.entry_id] },
      { ...state.review, opened: null, content: null }, entry.entry_id);
  }

  const refresh = () => { void read({ mode: 'manifest', card, expected_seq: expectedSeq }, null); };

  return <section className="diff-panel" aria-label="Changes">
    <h4>Changes</h4>
    {closed ? <>
      <p className="diff-note">Read the tracked files this round changed, side by side. Nothing is read until you ask.</p>
      <button type="button" onClick={refresh}>Show changes</button>
    </> : <>
      <div className="diff-actions">
        <button type="button" disabled={loading} onClick={refresh}>Refresh</button>
        <button type="button" onClick={() => { latest.current += 1; inflight.current = null; setState({ owner, phase: 'closed' }); }}>Hide changes</button>
      </div>
      <p className="diff-status" role="status">{loading ? 'Reading the checkout…' : state.phase === 'refused' ? failureText(state.reason) : ''}</p>
      {review === null ? null : <DiffReview review={review} loading={loading} onOpen={open}
        onPage={() => { if (review.page.next_cursor !== null) void read({ mode: 'page', manifest_id: review.page.manifest_id, cursor: review.page.next_cursor }, review); }} />}
      {review === null ? <p className="diff-note">For a local review, run <code>dudes ticket open {card}</code> in your terminal.</p> : null}
    </>}
  </section>;
}
