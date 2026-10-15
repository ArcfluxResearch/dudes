// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useId, useMemo, useRef, useState } from 'react';
import type { DiffContent, DiffEntry, DiffFreshness, DiffKind, DiffOmissionReason, DiffOutcome, DiffPage,
  DiffRefusalReason, DiffRequest, DiffTransport } from './diff.ts';
import { sameDiffBinding } from './diff.ts';
import './diff.css';

const FRESHNESS: Readonly<Record<DiffFreshness, string>> = {
  matches_review_sample: 'Matches the review sample taken after this check.',
  changed_after_check: 'The reviewed checkout content changed after this check.',
  unknown: 'Current checkout differences; content freshness against this check is unknown.',
};
const KINDS: Readonly<Record<DiffKind, string>> = {
  added: 'Added', modified: 'Modified', deleted: 'Deleted', renamed: 'Renamed', type_changed: 'Type changed',
};
const OMISSIONS: Readonly<Record<DiffOmissionReason, string>> = {
  binary: 'Binary content is not shown.',
  unsupported_text: 'This file contains text the preview cannot display.',
  sensitive_path: 'This path is excluded from source previews.',
  sensitive_content: 'This file matched the sensitive-content scan and is not shown.',
  oversize_file: 'This file exceeds the whole-file preview limit.',
  oversize_line: 'This file has a line longer than the preview limit.',
  unsupported_kind: 'This file kind has no source preview.',
  raced: 'This file changed while it was being read.',
  unreadable: 'This file could not be read.',
  response_limit: 'This complete file would exceed the response limit.',
};
const REFUSALS: Readonly<Record<DiffRefusalReason, string>> = {
  baseline_unavailable: 'The round-start baseline is unavailable. Use your editor to review the checkout.',
  stale_ticket: 'This ticket changed. Refresh differences to review its current round.',
  stale_manifest: 'This file selection is no longer valid. Refresh differences to choose files again.',
  expired: 'This file selection expired. Refresh differences to choose files again.',
  invalid_request: 'The daemon refused this file selection.',
  disabled: 'Source previews are disabled on this daemon.',
  busy: 'Another read is running, or this session read a moment ago. Refresh differences after a few seconds.',
  session_limit: 'This Floor session has used its source-preview allowance. Refreshing does not renew it. Use your editor to continue.',
  unsupported_platform: 'Source previews are unavailable on this daemon platform.',
  worktree_mismatch: 'The recorded worktree no longer matches its checkout.',
  unsupported_storage: 'This repository uses storage the source preview cannot read.',
  root_mismatch: 'The recorded checkout root changed. No source preview was returned.',
  timed_out: 'The source read did not finish in time. Refresh differences to try again.',
  git_unavailable: 'The daemon found no usable Git program for a source preview.',
  unreadable: 'The checkout could not be read for a source preview.',
};

export interface DiffReviewState {
  readonly page: DiffPage;
  readonly entries: readonly DiffEntry[];
  readonly selected: readonly string[];
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
  if (reason === 'forbidden') return 'This page has no current Floor session. No source preview was returned.';
  if (reason === 'unavailable') return 'The daemon did not answer. Refresh differences to try again.';
  if (reason === 'unknown') return 'The daemon sent a reply this page could not read. Refresh differences to try again.';
  return REFUSALS[reason];
}

function SourceLines({ label, text }: { readonly label: 'Old' | 'Current'; readonly text: string }) {
  const endsWithNewline = /[\r\n]$/.test(text);
  const lineCount = text === '' ? 0 : (text.match(/\r\n|\r|\n/g)?.length ?? 0) + (endsWithNewline ? 0 : 1);
  const numbers = Array.from({ length: lineCount }, (_, index) => index + 1).join('\n');
  return <section className="diff-side">
    <h6>{label}</h6>
    {text === '' ? <p className="diff-note">Empty side.</p> : <>
      <div className="diff-source" tabIndex={0} role="region" aria-label={label === 'Old' ? 'Old source lines' : 'Current source lines'}>
        <pre><span className="diff-line-number" aria-hidden="true">{numbers}</span><code>{text}</code></pre>
      </div>
      {!endsWithNewline ? <p className="diff-note">No final newline.</p> : null}
    </>}
  </section>;
}

function FileContent({ content, entries }: { readonly content: DiffContent; readonly entries: readonly DiffEntry[] }) {
  return <div className="diff-contents">
    {content.files.map((file) => <article className="diff-file" key={file.entry_id}>
      <h5><span className="diff-kind">{KINDS[file.kind]}</span> <span className="diff-path">{file.path}</span></h5>
      {file.old_path === null ? null : <p className="diff-note">Previously <span className="diff-path">{file.old_path}</span></p>}
      <SourceLines label="Old" text={file.old} /><SourceLines label="Current" text={file.current} />
    </article>)}
    {content.omitted.map((omission) => <p className="diff-omission" key={omission.entry_id}>
      <span className="diff-path">{entries.find((entry) => entry.entry_id === omission.entry_id)?.path}</span>{' '}
      {OMISSIONS[omission.reason]} No part of the file pair is shown.
    </p>)}
    {content.truncated ? <p className="diff-omission">The selected content response reached a limit. Some complete file pairs are not shown.</p> : null}
  </div>;
}

export function DiffReview({ review, loading, onSelect, onPage, onRead }: {
  readonly review: DiffReviewState;
  readonly loading: boolean;
  readonly onSelect: (id: string) => void;
  readonly onPage: () => void;
  readonly onRead: () => void;
}) {
  const id = useId();
  const { page, entries, selected, content } = review;
  const freshness = content?.freshness ?? page.freshness;
  return <>
    <p className={freshness === 'changed_after_check' ? 'diff-freshness diff-warning' : 'diff-freshness'}>{FRESHNESS[freshness]}</p>
    <p className="diff-note">Untracked and omitted files, and changes hidden from the bounded candidate read, are outside this comparison.</p>
    <p className="diff-note">These differences do not show what the check read or prove continuous content identity.</p>
    <p className="diff-note">Round {page.binding.round}. {page.binding.check_lsn === null ? 'No check sample is recorded.' : <>Associated check record {page.binding.check_lsn}.</>}</p>
    {page.binding.preexisting_tracked ? <p className="diff-warning">Tracked changes already existed before this round. These differences may include them.</p> : null}
    <p className="diff-note">{page.untracked_lower_bound ? 'At least ' : ''}{page.untracked} untracked {page.untracked === 1 ? 'file' : 'files'}. Their names and content are never shown.</p>
    {page.omitted > 0 || page.truncated ? <p className="diff-omission">{page.omitted_lower_bound ? 'At least ' : ''}{page.omitted} candidate {page.omitted === 1 ? 'file was' : 'files were'} omitted from the selection. {page.truncated ? 'The selection reached a limit.' : ''}</p> : null}
    <p className="diff-note">Choose up to 8 files, then read their content. Previews contain complete text pairs, at most 16 KiB per side and 4 KiB per line.</p>
    {entries.length === 0 ? <p>No eligible tracked differences were found within this read.</p> : <ul className="diff-selection">
      {entries.map((entry, index) => <li key={index}>
        <div><span className="diff-kind">{KINDS[entry.kind]}</span>{' '}<span className="diff-path" id={`${id}-${index}`}>{entry.path}</span>
          {entry.old_path === null ? null : <span className="diff-note"> Previously <span className="diff-path">{entry.old_path}</span></span>}</div>
        {entry.availability.state === 'unavailable' || entry.entry_id === null
          ? <p className="diff-note">{entry.availability.state === 'unavailable' ? OMISSIONS[entry.availability.reason] : ''}</p>
          : <button type="button" aria-describedby={`${id}-${index}`} aria-pressed={selected.includes(entry.entry_id)}
            disabled={loading || (!selected.includes(entry.entry_id) && selected.length >= 8)}
            onClick={() => { if (entry.entry_id !== null) onSelect(entry.entry_id); }}>{selected.includes(entry.entry_id) ? 'Deselect file' : 'Select file'}</button>}
      </li>)}
    </ul>}
    <div className="diff-actions">
      <button type="button" disabled={loading || selected.length === 0} onClick={onRead}>Read selected files</button>
      {page.next_cursor === null ? null : <button type="button" disabled={loading} onClick={onPage}>Next files</button>}
      <span className="diff-note">{selected.length} of 8 selected.</span>
    </div>
    {content === null ? null : <FileContent content={content} entries={entries} />}
    <p className="diff-note">For omitted files or a larger review, run <code>dudes ticket open {page.binding.card}</code> in your terminal.</p>
  </>;
}

function matchesReview(review: DiffReviewState, response: DiffPage | DiffContent): boolean {
  return response.manifest_id === review.page.manifest_id && response.expires_at === review.page.expires_at
    && sameDiffBinding(response.binding, review.page.binding);
}

function validContent(review: DiffReviewState, content: DiffContent): boolean {
  const ids = [...content.files.map((file) => file.entry_id), ...content.omitted.map((row) => row.entry_id)];
  return new Set(ids).size === ids.length && ids.every((id) => review.selected.includes(id))
    && ids.length === review.selected.length
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
  const [state, setState] = useState<PanelState>({ owner, phase: 'closed' });
  useEffect(() => () => { latest.current += 1; }, [owner]);
  const mine = state.owner === owner;
  const closed = !mine || state.phase === 'closed';
  const loading = mine && state.phase === 'loading';
  const review = mine && (state.phase === 'ready' || state.phase === 'loading') ? state.review : null;

  async function read(request: DiffRequest, previous: DiffReviewState | null): Promise<void> {
    if (loading) return;
    const ticket = ++latest.current;
    const generation = transport.generation();
    setState({ owner, phase: 'loading', review: previous });
    let outcome: DiffOutcome;
    try { outcome = await transport.request(request); } catch { outcome = { kind: 'unavailable' }; }
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
      if (previous === null || !validContent(previous, reply)) { setState({ owner, phase: 'refused', reason: 'unknown' }); return; }
      setState({ owner, phase: 'ready', review: { ...previous, content: reply } }); return;
    }
    const entries = [...(previous?.entries ?? []), ...reply.entries];
    const ids = entries.flatMap((entry) => entry.entry_id === null ? [] : [entry.entry_id]);
    if (entries.length > 256 || new Set(ids).size !== ids.length) { setState({ owner, phase: 'refused', reason: 'unknown' }); return; }
    setState({ owner, phase: 'ready', review: { page: reply, entries, selected: previous?.selected ?? [], content: previous?.content ?? null } });
  }

  function select(id: string): void {
    if (state.phase !== 'ready' || !mine) return;
    const selected = state.review.selected.includes(id) ? state.review.selected.filter((value) => value !== id)
      : state.review.selected.length < 8 ? [...state.review.selected, id] : state.review.selected;
    setState({ owner, phase: 'ready', review: { ...state.review, selected, content: null } });
  }

  return <section className="diff-panel" aria-label="Checkout differences">
    <h4>Checkout differences</h4>
    {closed ? <>
      <p className="diff-note">Review bounded tracked text changes against this round&apos;s start commit.</p>
      <button type="button" onClick={() => { void read({ mode: 'manifest', card, expected_seq: expectedSeq }, null); }}>Open differences</button>
    </> : <>
      <div className="diff-actions">
        <button type="button" disabled={loading} onClick={() => { void read({ mode: 'manifest', card, expected_seq: expectedSeq }, null); }}>Refresh differences</button>
        <button type="button" onClick={() => { latest.current += 1; setState({ owner, phase: 'closed' }); }}>Close differences</button>
      </div>
      <p className="diff-status" role="status">{loading ? 'Reading checkout differences...' : state.phase === 'refused' ? failureText(state.reason) : ''}</p>
      {review === null ? null : <DiffReview review={review} loading={loading} onSelect={select}
        onPage={() => { if (review.page.next_cursor !== null) void read({ mode: 'page', manifest_id: review.page.manifest_id, cursor: review.page.next_cursor }, review); }}
        onRead={() => { if (review.selected.length > 0) void read({ mode: 'content', manifest_id: review.page.manifest_id, entries: review.selected }, review); }} />}
      {review === null ? <p className="diff-note">For a local review, run <code>dudes ticket open {card}</code> in your terminal.</p> : null}
    </>}
  </section>;
}
