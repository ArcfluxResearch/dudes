// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Git tab: one agent's branch, recent commits, changed files and distance from the remote
// default branch, or from the commit its worktree started from, read on demand.
//
// Every string is drawn as text and none is put in a title, an ARIA label or a data attribute.
// The page reads the checkout when the tab opens and when the operator presses Refresh, and
// at no other time.

import { Button, cn } from '../../ui/index.ts';
import { RelativeTime } from '../RelativeTime.tsx';
import { useEffect, useId, useState, useSyncExternalStore } from 'react';

import { browserControlDeps } from '../control/control.ts';
import type { ControlDeps } from '../control/control.ts';
import { plural } from '../format.ts';
import type { AgentRow } from '../scene/cast.ts';
import { HEADING, NOTE as BASE_NOTE } from '../chat/look.ts';
import { getGitState, loadGit, needsRead, copyablePath, refusalText, subscribeToGit } from './git.ts';
import type { GitAgainst, GitChangeKind, GitHead, GitView } from './git.ts';

const NOTE = cn('git-note', BASE_NOTE);
const LIST = 'm-0 list-none p-0';
const ROW = 'flex flex-wrap items-center gap-x-2 gap-y-1 border-t border-line py-1.5 text-small [overflow-wrap:anywhere]';
const DATA = 'data font-mono text-small';

const SECTION = 'flex flex-col gap-1';
const NONE = 'm-0 text-small text-muted';
const KIND = 'git-kind rounded-sm bg-surface-2 px-1.5 py-0.5 text-caption font-medium text-muted';

const KIND_LABEL: Readonly<Record<GitChangeKind, string>> = Object.freeze({
  added: 'Added', modified: 'Modified', deleted: 'Deleted', renamed: 'Renamed',
  type_changed: 'Type changed', conflicted: 'Conflict',
});

interface Comparison {
  readonly base: string;
  readonly missing: string;
  readonly commits: string;
  readonly files: string;
}

/** What the daemon compared with, said the same way in every heading and sentence. */
const AGAINST: Readonly<Record<GitAgainst, Comparison>> = Object.freeze({
  remote_default_branch: Object.freeze({
    base: 'the remote default branch',
    missing: 'No remote default branch is recorded here to compare with.',
    commits: 'Commits not on the remote default branch',
    files: 'Files committed since leaving the default branch',
  }),
  worktree_start: Object.freeze({
    base: 'the commit this worktree started from',
    missing: 'The commit this worktree started from is no longer in the repository.',
    commits: 'Commits since this worktree started',
    files: 'Files committed since this worktree started',
  }),
});

function Cut({ shown, truncated, dropped }: { readonly shown: number; readonly truncated: boolean; readonly dropped: number }) {
  if (!truncated && dropped === 0) return null;
  return <p className={cn('git-cut m-0 text-small text-muted')}>
    {truncated ? `Showing the first ${plural(shown, 'entry', 'entries')}; the rest are not shown. ` : ''}
    {dropped === 0 ? '' : `${plural(dropped, 'entry was', 'entries were')} left out because ${dropped === 1 ? 'its text' : 'their text'} held control or formatting characters.`}
  </p>;
}

function Path({ id, path, cut, onCopy }: {
  readonly id: string;
  readonly path: string;
  readonly cut: boolean;
  readonly onCopy: (path: string) => void;
}) {
  return <>
    <span className={cn('git-path min-w-0 flex-1 [overflow-wrap:anywhere]', DATA)} id={id}>{path}{cut ? '…' : ''}</span>
    {/* The path is the description, never the label: the label would put repository text in an attribute. */}
    <Button size="sm" variant="ghost" className="git-copy" aria-describedby={id} disabled={cut}
      title={cut ? 'The path is shortened. Copy the complete path from your editor.' : undefined}
      onClick={() => onCopy(path)}>Copy path</Button>
  </>;
}

function Head({ head }: { readonly head: GitHead }) {
  const against = AGAINST[head.against];
  const base = head.ahead === null || head.behind === null
    ? against.missing
    : `${plural(head.ahead, 'commit', 'commits')} ahead of and ${plural(head.behind, 'commit', 'commits')} behind ${against.base}.`;
  const cell = 'flex min-w-0 flex-col gap-0.5 rounded-md bg-surface-2 px-3 py-2';
  const term = 'text-caption font-semibold tracking-label text-muted uppercase';
  const value = 'm-0 text-base [overflow-wrap:anywhere]';
  return <dl className="git-head m-0 grid grid-cols-2 gap-2">
    <div className={cell}><dt className={term}>Branch</dt><dd className={value}>{head.branch ?? 'Detached HEAD'}</dd></div>
    <div className={cell}><dt className={term}>Commit</dt><dd className={cn(value, DATA)}>{head.id ?? 'No commit yet'}</dd></div>
    <div className={cn(cell, 'col-span-2')}><dt className={term}>Distance</dt><dd className={value}>{base}</dd></div>
    <div className={cn(cell, 'col-span-2')}><dt className={term}>Untracked files</dt>
      <dd className={value}>{head.untracked_cut ? `At least ${head.untracked.toLocaleString()}` : head.untracked.toLocaleString()}
        <span className={NOTE}> Their names are never shown.</span></dd></div>
  </dl>;
}

function Branches({ view }: { readonly view: GitView }) {
  const sectionId = useId();
  const { items, truncated, dropped } = view.branches;
  return <section aria-labelledby={`${sectionId}-branches`} className={SECTION}>
    <h4 id={`${sectionId}-branches`} className={HEADING}>Local branches</h4>
    {items.length === 0 ? <p className={NONE}>None.</p> : <ul className={LIST}>
      {items.map((branch, index) => <li key={index} className={ROW}>
        <span>{branch.name}</span>{branch.current ? <span className={NOTE}>(current)</span> : null}
        <span className={cn(DATA, 'ms-auto text-muted')}>{branch.tip}</span>
      </li>)}
    </ul>}
    <Cut shown={items.length} truncated={truncated} dropped={dropped} />
  </section>;
}

function Commits({ view, against }: { readonly view: GitView; readonly against: Comparison }) {
  const sectionId = useId();
  const { items, truncated, dropped } = view.commits;
  return <section aria-labelledby={`${sectionId}-commits`} className={SECTION}>
    <h4 id={`${sectionId}-commits`} className={HEADING}>{against.commits}</h4>
    {items.length === 0 ? <p className={NONE}>None.</p> : <ol className={LIST}>
      {items.map((commit, index) => <li key={index} className={ROW}>
        <span className="min-w-0 flex-1">{commit.subject}</span>
        <span className={cn(DATA, 'text-muted')}>{commit.id}</span>
        <RelativeTime className={NOTE} at={commit.at} />
      </li>)}
    </ol>}
    <Cut shown={items.length} truncated={truncated} dropped={dropped} />
  </section>;
}

function Files({ view, against, onCopy }: {
  readonly view: GitView;
  readonly against: Comparison;
  readonly onCopy: (path: string) => void;
}) {
  const sectionId = useId();
  const committed = view.committed;
  const changes = view.changes;
  return <>
    <section aria-labelledby={`${sectionId}-committed`} className={SECTION}>
      <h4 id={`${sectionId}-committed`} className={HEADING}>{against.files}</h4>
      {committed.items.length === 0 ? <p className={NONE}>None.</p> : <ul className={LIST}>
        {committed.items.map((file, index) => <li key={index} className={ROW}>
          <span className={KIND}>{KIND_LABEL[file.kind]}</span>
          <Path id={`${sectionId}-committed-${index}`} path={file.path} cut={file.cut} onCopy={onCopy} />
        </li>)}
      </ul>}
      <Cut shown={committed.items.length} truncated={committed.truncated} dropped={committed.dropped} />
    </section>
    <section aria-labelledby={`${sectionId}-changes`} className={SECTION}>
      <h4 id={`${sectionId}-changes`} className={HEADING}>Uncommitted changes to tracked files</h4>
      {changes.items.length === 0 ? <p className={NONE}>None.</p> : <ul className={LIST}>
        {changes.items.map((file, index) => <li key={index} className={ROW}>
          <span className={KIND}>{KIND_LABEL[file.kind]}{file.staged ? ', staged' : ''}</span>
          <Path id={`${sectionId}-change-${index}`} path={file.path} cut={file.cut} onCopy={onCopy} />
        </li>)}
      </ul>}
      <Cut shown={changes.items.length} truncated={changes.truncated} dropped={changes.dropped} />
    </section>
  </>;
}

/**
 * `active` is whether the Git tab is the one showing. The checkout is read the first time it
 * is, and again only when the operator presses Refresh.
 */
export function GitPanel({ row, active, deps = browserControlDeps }: {
  readonly row: AgentRow;
  readonly active: boolean;
  readonly deps?: ControlDeps;
}) {
  const git = useSyncExternalStore(subscribeToGit, getGitState, getGitState);
  const [copied, setCopied] = useState<string | null>(null);
  useEffect(() => {
    if (needsRead(getGitState(), row.id, active)) void loadGit(row.id, deps);
  }, [active, row.id, deps]);
  const mine = git.agent === row.id;
  const view = mine ? git.view : null;
  const loading = mine && git.phase === 'loading';
  const problem = mine ? refusalText(view, git.failure, git.busy) : null;

  function copy(path: string): void {
    // Explicitly relative and quoted, so a pasted path is one word that no option or editor command can claim; the screen shows it raw.
    navigator.clipboard.writeText(copyablePath(path)).then(
      () => setCopied('Copied the path, relative and in single quotes.'),
      () => setCopied('The browser did not allow copying.'),
    );
  }

  return <section className="git-panel flex flex-col gap-3 font-sans text-base text-ink" aria-label="Git">
    <div className="flex items-center justify-between gap-3">
      <h3 className="m-0 text-title font-semibold">Git</h3>
      <Button size="sm" className="git-refresh" aria-disabled={loading || undefined}
        title={loading ? 'Wait for the checkout to finish loading.' : undefined}
        onClick={() => { if (!loading) { setCopied(null); void loadGit(row.id, deps); } }}>Refresh</Button>
    </div>
    <p className={NOTE}>
      A read of this agent&rsquo;s recorded repositories, taken when you open this tab or press Refresh. It names tracked
      files and local branches, never untracked files, authors or where the checkout is. You read it. Dudes never acts on it.
    </p>
    <p className="git-status m-0 min-h-5 text-small text-muted empty:hidden" role="status">
      {loading ? 'Reading the checkout…' : problem ?? (view === null ? '' : <>Read <RelativeTime at={view.read_at} />.</>)}
      {copied === null ? null : <span> {copied}</span>}
    </p>
    {(view?.repositories ?? (view === null ? [] : [{ repo_id: null, view }])).map((repo) => <section key={repo.repo_id ?? 'legacy'} className="flex min-w-0 flex-col gap-3">
      {repo.repo_id === null ? null : <h4 className={HEADING}>{repo.repo_id}</h4>}
      {refusalText(repo.view, null) === null ? null : <p className={NOTE}>{refusalText(repo.view, null)}</p>}
      {repo.view.head === null ? null : <>
      <Head head={repo.view.head} />
      <Branches view={repo.view} />
      <Commits view={repo.view} against={AGAINST[repo.view.head.against]} />
      <Files view={repo.view} against={AGAINST[repo.view.head.against]} onCopy={copy} />
    </>}
    </section>)}
  </section>;
}
