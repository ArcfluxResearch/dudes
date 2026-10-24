// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Git tab: one agent's branch, recent commits, changed files and distance from the remote
// default branch, or from the commit its worktree started from, read on demand.
//
// Every string is drawn as text and none is put in a title, an ARIA label or a data attribute.
// The page reads the checkout when the tab opens and when the operator presses Refresh, and
// at no other time.

import { RelativeTime } from '../RelativeTime.tsx';
import { useEffect, useState, useSyncExternalStore } from 'react';

import { browserControlDeps } from '../control/control.ts';
import type { ControlDeps } from '../control/control.ts';
import { plural } from '../format.ts';
import type { AgentRow } from '../scene/cast.ts';
import { getGitState, loadGit, needsRead, copyablePath, refusalText, subscribeToGit } from './git.ts';
import type { GitAgainst, GitChangeKind, GitHead, GitView } from './git.ts';

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
  return <p className="git-cut">
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
    <span className="git-path data" id={id}>{path}{cut ? '…' : ''}</span>
    {/* The path is the description, never the label: the label would put repository text in an attribute. */}
    <button type="button" className="git-copy" aria-describedby={id} disabled={cut}
      title={cut ? 'The path is shortened. Copy the complete path from your editor.' : undefined}
      onClick={() => onCopy(path)}>Copy path</button>
  </>;
}

function Head({ head }: { readonly head: GitHead }) {
  const against = AGAINST[head.against];
  const base = head.ahead === null || head.behind === null
    ? against.missing
    : `${plural(head.ahead, 'commit', 'commits')} ahead of and ${plural(head.behind, 'commit', 'commits')} behind ${against.base}.`;
  return <dl className="git-head">
    <div><dt>Branch</dt><dd>{head.branch ?? 'Detached HEAD'}</dd></div>
    <div><dt>Commit</dt><dd className="data">{head.id ?? 'No commit yet'}</dd></div>
    <div><dt>Distance</dt><dd>{base}</dd></div>
    <div><dt>Untracked files</dt><dd>{head.untracked_cut ? `At least ${head.untracked.toLocaleString()}` : head.untracked.toLocaleString()}
      <span className="git-note"> Their names are never shown.</span></dd></div>
  </dl>;
}

function Branches({ view }: { readonly view: GitView }) {
  const { items, truncated, dropped } = view.branches;
  return <section aria-labelledby="git-branches-title">
    <h4 id="git-branches-title">Local branches</h4>
    {items.length === 0 ? <p>None.</p> : <ul>
      {items.map((branch, index) => <li key={index}>
        <span>{branch.name}</span>{branch.current ? <span className="git-note"> (current)</span> : null}
        {' '}<span className="data">{branch.tip}</span>
      </li>)}
    </ul>}
    <Cut shown={items.length} truncated={truncated} dropped={dropped} />
  </section>;
}

function Commits({ view, against }: { readonly view: GitView; readonly against: Comparison }) {
  const { items, truncated, dropped } = view.commits;
  return <section aria-labelledby="git-commits-title">
    <h4 id="git-commits-title">{against.commits}</h4>
    {items.length === 0 ? <p>None.</p> : <ol>
      {items.map((commit, index) => <li key={index}>
        <span>{commit.subject}</span>{' '}
        <span className="data">{commit.id}</span>{' '}
        <RelativeTime className="git-note" at={commit.at} />
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
  const committed = view.committed;
  const changes = view.changes;
  return <>
    <section aria-labelledby="git-committed-title">
      <h4 id="git-committed-title">{against.files}</h4>
      {committed.items.length === 0 ? <p>None.</p> : <ul>
        {committed.items.map((file, index) => <li key={index}>
          <span className="git-kind">{KIND_LABEL[file.kind]}</span>{' '}
          <Path id={`git-committed-${index}`} path={file.path} cut={file.cut} onCopy={onCopy} />
        </li>)}
      </ul>}
      <Cut shown={committed.items.length} truncated={committed.truncated} dropped={committed.dropped} />
    </section>
    <section aria-labelledby="git-changes-title">
      <h4 id="git-changes-title">Uncommitted changes to tracked files</h4>
      {changes.items.length === 0 ? <p>None.</p> : <ul>
        {changes.items.map((file, index) => <li key={index}>
          <span className="git-kind">{KIND_LABEL[file.kind]}{file.staged ? ', staged' : ''}</span>{' '}
          <Path id={`git-change-${index}`} path={file.path} cut={file.cut} onCopy={onCopy} />
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

  return <section className="agent-detail-git" aria-label="Git">
    <h3>Git</h3>
    <p className="git-note">
      A read of this agent&rsquo;s checkout, taken when you open this tab or press Refresh. It names tracked
      files and local branches, never untracked files, authors or where the checkout is. You read it. Dudes never acts on it.
    </p>
    <button type="button" className="git-refresh" aria-disabled={loading || undefined}
      title={loading ? 'Wait for the checkout to finish loading.' : undefined}
      onClick={() => { if (!loading) { setCopied(null); void loadGit(row.id, deps); } }}>Refresh</button>
    <p className="git-status" role="status">
      {loading ? 'Reading the checkout…' : problem ?? (view === null ? '' : <>Read <RelativeTime at={view.read_at} />.</>)}
      {copied === null ? null : <span> {copied}</span>}
    </p>
    {view === null || view.head === null ? null : <>
      <Head head={view.head} />
      <Branches view={view} />
      <Commits view={view} against={AGAINST[view.head.against]} />
      <Files view={view} against={AGAINST[view.head.against]} onCopy={copy} />
    </>}
  </section>;
}
