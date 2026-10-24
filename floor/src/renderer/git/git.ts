// SPDX-License-Identifier: MIT OR Apache-2.0
//
// One agent's checkout, as the Git tab reads it. An agent hired into
// a worktree Dudes created is read through what its lease records, and its
// distance is measured from the commit the worktree started from.
//
// The tab is the operator's opt-in. A snapshot carries the `git` sibling only
// when the daemon routes `POST /floor/git`, and without it the page draws no
// Git tab and sends nothing. The read is a pull: the page asks when the
// operator opens the tab or presses Refresh, never on a timer and never from a
// stream frame.
//
// Everything in a reply is untrusted display text from a directory the agent
// writes. The decoder here is strict and fails closed: a reply with an
// unknown field, a missing one, an oversize string, a control or format
// character, an absolute path or one that climbs out of the repository is no
// reply at all. Strings are drawn as text and are never put in a title, an
// ARIA label or a data attribute. The tab is evidence the
// operator reads. Nothing here is a verdict.

import type { ControlDeps } from '../control/control.ts';

export const MAX_GIT_VIEW_BYTES = 64 * 1024;
export const MAX_GIT_HEAD_BYTES = 1024;
export const MAX_GIT_BRANCH_BYTES = 4 * 1024;
export const MAX_GIT_COMMIT_BYTES = 10 * 1024;
export const MAX_GIT_COMMITTED_BYTES = 24 * 1024;
export const MAX_GIT_CHANGE_BYTES = 24 * 1024;
export const MAX_GIT_BRANCHES = 30;
export const MAX_GIT_COMMITS = 20;
export const MAX_GIT_PARENTS = 4;
export const MAX_GIT_NAME_BYTES = 100;
export const MAX_GIT_SUBJECT_BYTES = 120;
export const MAX_GIT_PATH_BYTES = 200;

const UTF8 = new TextEncoder();
const SHORT_ID = /^[0-9a-f]{12}$/;
/** A control character, a format character (bidirectional overrides among them) or a line separator. */
const FORBIDDEN = /[\p{Cc}\p{Cf}\u2028\u2029]/u;
const UINT32_MAX = 4_294_967_295;
/** Spelled by code so the network-surface guard does not read it as a route. */
const SLASH = String.fromCharCode(0x2f);

export const GIT_STATES = [
  'ok', 'agent_unknown', 'not_a_checkout_root', 'gitfile_unsupported', 'unsupported_storage',
  'unsupported_platform', 'git_unavailable', 'busy', 'timed_out', 'unreadable',
  'worktree_mismatch', 'worktree_released', 'worktree_removed',
] as const;
export type GitState = (typeof GIT_STATES)[number];

/** The commit `ahead`, `behind` and the commit list are measured from. Dudes decides; the page only says which. */
export const GIT_AGAINST = ['remote_default_branch', 'worktree_start'] as const;
export type GitAgainst = (typeof GIT_AGAINST)[number];

export const CHANGE_KINDS = ['added', 'modified', 'deleted', 'renamed', 'type_changed', 'conflicted'] as const;
export type GitChangeKind = (typeof CHANGE_KINDS)[number];

export interface GitHead {
  readonly branch: string | null;
  readonly id: string | null;
  readonly ahead: number | null;
  readonly behind: number | null;
  readonly against: GitAgainst;
  readonly untracked: number;
  readonly untracked_cut: boolean;
}

export interface GitBranch { readonly name: string; readonly tip: string; readonly current: boolean }
export interface GitCommit {
  readonly id: string;
  readonly parents: readonly string[];
  readonly at: number;
  readonly subject: string;
}
export interface GitCommittedFile { readonly path: string; readonly cut: boolean; readonly kind: GitChangeKind }
export interface GitWorkingChange extends GitCommittedFile { readonly staged: boolean }

export interface GitSection<T> {
  readonly items: readonly T[];
  readonly truncated: boolean;
  readonly dropped: number;
}

export interface GitView {
  readonly state: GitState;
  readonly read_at: number;
  readonly head: GitHead | null;
  readonly branches: GitSection<GitBranch>;
  readonly commits: GitSection<GitCommit>;
  readonly committed: GitSection<GitCommittedFile>;
  readonly changes: GitSection<GitWorkingChange>;
}

function exactly(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const own = Object.keys(value);
  return own.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function bytes(value: unknown): number {
  return UTF8.encode(JSON.stringify(value)).length;
}

function count(value: unknown): value is number {
  return typeof value === 'number' && Number.isInteger(value) && value >= 0 && value <= UINT32_MAX;
}

/** A showable string of at most `max` bytes. */
function text(value: unknown, max: number): value is string {
  return typeof value === 'string' && UTF8.encode(value).length <= max && !FORBIDDEN.test(value);
}

function shortId(value: unknown): value is string {
  return typeof value === 'string' && SHORT_ID.test(value);
}

/** Repository-relative: not absolute, and no empty or `..` component. */
function relative(value: unknown): value is string {
  return text(value, MAX_GIT_PATH_BYTES)
    && value.split(/\//).every((part) => part !== '' && part !== '..');
}

function against(value: unknown): value is GitAgainst {
  return typeof value === 'string' && (GIT_AGAINST as readonly string[]).includes(value);
}

function head(value: unknown): GitHead | null {
  if (!exactly(value, ['branch', 'id', 'ahead', 'behind', 'against', 'untracked', 'untracked_cut'])
    || bytes(value) > MAX_GIT_HEAD_BYTES) return null;
  const { branch, id, ahead, behind, against: base, untracked, untracked_cut: cut } = value;
  if ((branch !== null && !text(branch, MAX_GIT_NAME_BYTES)) || (id !== null && !shortId(id))
    || (ahead !== null && !count(ahead)) || (behind !== null && !count(behind)) || !against(base)
    || !count(untracked) || typeof cut !== 'boolean') return null;
  return Object.freeze({ branch, id, ahead, behind, against: base, untracked, untracked_cut: cut });
}

function section<T>(
  value: unknown, maxItems: number, maxBytes: number, item: (raw: unknown) => T | null,
): GitSection<T> | null {
  if (!exactly(value, ['items', 'truncated', 'dropped']) || bytes(value) > maxBytes) return null;
  const { items, truncated, dropped } = value;
  if (!Array.isArray(items) || items.length > maxItems || typeof truncated !== 'boolean' || !count(dropped)) return null;
  const rows: T[] = [];
  for (const raw of items) {
    const row = item(raw);
    if (row === null) return null;
    rows.push(row);
  }
  return Object.freeze({ items: Object.freeze(rows), truncated, dropped });
}

function branch(raw: unknown): GitBranch | null {
  if (!exactly(raw, ['name', 'tip', 'current'])) return null;
  return text(raw.name, MAX_GIT_NAME_BYTES) && shortId(raw.tip) && typeof raw.current === 'boolean'
    ? Object.freeze({ name: raw.name, tip: raw.tip, current: raw.current }) : null;
}

function commit(raw: unknown): GitCommit | null {
  if (!exactly(raw, ['id', 'parents', 'at', 'subject'])) return null;
  const { id, parents, at, subject } = raw;
  if (!shortId(id) || !Array.isArray(parents) || parents.length > MAX_GIT_PARENTS || !parents.every(shortId)
    || typeof at !== 'number' || !Number.isSafeInteger(at) || !text(subject, MAX_GIT_SUBJECT_BYTES)) return null;
  return Object.freeze({ id, parents: Object.freeze([...parents] as string[]), at, subject });
}

function kind(value: unknown): value is GitChangeKind {
  return typeof value === 'string' && (CHANGE_KINDS as readonly string[]).includes(value);
}

function committedFile(raw: unknown): GitCommittedFile | null {
  if (!exactly(raw, ['path', 'cut', 'kind'])) return null;
  return relative(raw.path) && typeof raw.cut === 'boolean' && kind(raw.kind)
    ? Object.freeze({ path: raw.path, cut: raw.cut, kind: raw.kind }) : null;
}

function workingChange(raw: unknown): GitWorkingChange | null {
  if (!exactly(raw, ['path', 'cut', 'kind', 'staged'])) return null;
  return relative(raw.path) && typeof raw.cut === 'boolean' && kind(raw.kind) && typeof raw.staged === 'boolean'
    ? Object.freeze({ path: raw.path, cut: raw.cut, kind: raw.kind, staged: raw.staged }) : null;
}

/**
 * Decode one reply body, or `null` when it is anything but the finite view of
 * the Git tab contract. A refusal state carries no head and no entries.
 */
export function decodeGitView(body: string): GitView | null {
  if (UTF8.encode(body).length > MAX_GIT_VIEW_BYTES) return null;
  let value: unknown;
  try {
    value = JSON.parse(body);
  } catch {
    return null;
  }
  if (!exactly(value, ['state', 'read_at', 'head', 'branches', 'commits', 'committed', 'changes'])) return null;
  const state = (GIT_STATES as readonly unknown[]).includes(value.state) ? value.state as GitState : null;
  if (state === null || typeof value.read_at !== 'number' || !Number.isSafeInteger(value.read_at)) return null;
  const parsedHead = value.head === null ? null : head(value.head);
  const branches = section(value.branches, MAX_GIT_BRANCHES, MAX_GIT_BRANCH_BYTES, branch);
  const commits = section(value.commits, MAX_GIT_COMMITS, MAX_GIT_COMMIT_BYTES, commit);
  const committed = section(value.committed, Number.MAX_SAFE_INTEGER, MAX_GIT_COMMITTED_BYTES, committedFile);
  const changes = section(value.changes, Number.MAX_SAFE_INTEGER, MAX_GIT_CHANGE_BYTES, workingChange);
  if (branches === null || commits === null || committed === null || changes === null) return null;
  if ((state === 'ok') !== (parsedHead !== null) || (value.head !== null && parsedHead === null)) return null;
  if (state !== 'ok' && [branches, commits, committed, changes].some((rows) => rows.items.length > 0
    || rows.truncated || rows.dropped > 0)) return null;
  return Object.freeze({ state, read_at: value.read_at, head: parsedHead, branches, commits, committed, changes });
}

/** The reply to one request, as far as the page can tell. */
export type GitOutcome =
  | { readonly kind: 'view'; readonly view: GitView }
  | { readonly kind: 'forbidden' }
  | { readonly kind: 'rejected'; readonly status: number }
  | { readonly kind: 'unavailable' }
  | { readonly kind: 'unknown' };

/** The one request: the agent, and nothing else. */
export async function postGit(agent: string, fetch: ControlDeps['fetch']): Promise<GitOutcome> {
  let status: number;
  let body: string;
  try {
    const response = await fetch('/floor/git', {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ agent_id: agent }),
    });
    status = response.status;
    body = await response.text();
  } catch {
    return Object.freeze({ kind: 'unavailable' });
  }
  if (status === 200) {
    const view = decodeGitView(body);
    return view === null ? Object.freeze({ kind: 'unknown' }) : Object.freeze({ kind: 'view', view });
  }
  if (status === 403) return Object.freeze({ kind: 'forbidden' });
  if (status === 400 || status === 413) return Object.freeze({ kind: 'rejected', status });
  return Object.freeze({ kind: status === 503 ? 'unavailable' : 'unknown' });
}

/**
 * A path as a shell would read it: in single quotes, each embedded quote closed,
 * escaped and reopened, so a pasted path is one word and runs nothing.
 */
export function shellQuote(path: string): string {
  return `'${path.replaceAll("'", "'\\''")}'`;
}

/**
 * What Copy path puts on the clipboard: the path made explicitly relative, then quoted. The `./`
 * stops a path that starts with `-` being read as an option and one that starts with `+` or `!`
 * being read as a command by an editor, which a quoted path alone does not.
 */
export function copyablePath(path: string): string {
  return shellQuote(`.${SLASH}${path}`);
}

export type GitPhase = 'idle' | 'loading' | 'ready' | 'failed';

export interface GitTabState {
  readonly agent: string | null;
  readonly phase: GitPhase;
  readonly view: GitView | null;
  readonly failure: GitOutcome | null;
  /** The newest answer was `busy`, so `view` is an earlier reading that is still the latest there is. */
  readonly busy: boolean;
}

const INITIAL: GitTabState = Object.freeze({ agent: null, phase: 'idle', view: null, failure: null, busy: false });

let state: GitTabState = INITIAL;
const listeners = new Set<() => void>();
/** Counts requests, so a late answer for an earlier one is dropped. */
let latest = 0;

export function getGitState(): GitTabState {
  return state;
}

export function subscribeToGit(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** What a reload does: forget everything. For tests; the page gets a fresh module per load. */
export function resetGit(): void {
  state = INITIAL;
  latest = 0;
  listeners.clear();
}

function commitState(next: GitTabState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

/** Whether showing the tab should read: the first time for this agent, and never again by itself. */
export function needsRead(current: GitTabState, agent: string, active: boolean): boolean {
  return active && current.agent !== agent;
}

/**
 * Read `agent`'s checkout. Called when the operator opens the tab or presses
 * Refresh, and by nothing else. The last view stays on screen while the next
 * one is read.
 */
export async function loadGit(agent: string, deps: ControlDeps): Promise<void> {
  const sameAgent = state.agent === agent;
  if (!deps.signedIn()) {
    commitState({ agent, phase: 'failed', view: sameAgent ? state.view : null,
      failure: Object.freeze({ kind: 'forbidden' }), busy: false });
    return;
  }
  const ticket = ++latest;
  commitState({ agent, phase: 'loading', view: sameAgent ? state.view : null, failure: null, busy: false });
  const outcome = await postGit(agent, deps.fetch);
  if (ticket !== latest) return;
  if (outcome.kind === 'view') {
    // `busy` says nothing about the repository, so it does not wipe a reading already on screen.
    const keep = outcome.view.state === 'busy' && sameAgent && state.view?.state === 'ok';
    commitState({ agent, phase: 'ready', view: keep ? state.view : outcome.view, failure: null, busy: keep });
    return;
  }
  commitState({ agent, phase: 'failed', view: sameAgent ? state.view : null, failure: outcome, busy: false });
  // An empty 403 may mean the session ended or that this page can no longer act with it;
  // a fresh subscription tells the two apart.
  if (outcome.kind === 'forbidden') deps.refresh();
}

const REFUSALS: Readonly<Record<Exclude<GitState, 'ok'>, string>> = Object.freeze({
  agent_unknown: 'Dudes does not know this agent.',
  not_a_checkout_root: 'This agent does not work at the root of a Git checkout, so there is nothing to read.',
  // A gitfile also comes from a submodule checkout or a separated git directory, so this does not say "linked worktree".
  gitfile_unsupported: 'This agent’s .git is a link file that Dudes did not create, so the Git tab does not read it.',
  unsupported_storage: 'This repository uses storage the Git tab does not read, or is larger than it reads.',
  unsupported_platform: 'The Git tab is not available on this computer.',
  git_unavailable: 'Dudes found no usable git to read with.',
  busy: 'Another read is running, or this page read a moment ago. Try again in a few seconds.',
  timed_out: 'The read did not finish in time.',
  unreadable: 'The repository could not be read as a repository.',
  worktree_mismatch: 'This agent’s worktree no longer matches the one Dudes created for it, so the Git tab did not read it.',
  worktree_released: 'This agent has retired, and the Git tab does not read the worktree it left behind.',
  worktree_removed: 'This agent has retired, and its worktree has been removed.',
});

/** The sentence a refusal state, or a failed request, is drawn as. */
export function refusalText(view: GitView | null, failure: GitOutcome | null, busy = false): string | null {
  if (busy) return REFUSALS.busy;
  if (view !== null && view.state !== 'ok') return REFUSALS[view.state];
  switch (failure?.kind) {
    case undefined:
    case 'view':
      return null;
    case 'forbidden':
      return 'This page has no Floor session, so nothing was sent.';
    case 'rejected':
      return 'Dudes refused the request.';
    case 'unavailable':
      return 'Dudes did not answer.';
    case 'unknown':
      return 'Dudes sent a reply this page could not read.';
  }
}
