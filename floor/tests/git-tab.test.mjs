// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Git tab: the strict decoder, the one request, what the operator reads and what
// they cannot be made to read or paste.
//
// Mutation controls, each named by the test it turns red:
//   - an unknown member accepted by the decoder ........ "the decoder accepts the daemon's view…"
//   - an absolute path accepted ......................... "the decoder refuses…"
//   - a format character accepted ....................... "the decoder refuses…"
//   - a refusal state carrying entries accepted ......... "the decoder refuses…"
//   - a head without its comparison accepted ............ "the decoder refuses…"
//   - worktree copy shown for the remote default branch . "a worktree read is measured from…"
//   - shellQuote left raw ............................... "a quoted path is one shell word…"
//   - a repository string put in an attribute .......... "no repository string reaches an attribute…"
//   - the tab drawn without the git sibling ............ "the tab exists only when the daemon sent the sibling"
//   - a read made by the effect after the first ........ "showing the tab reads once…"

import { hooks } from './helpers/hook-markup.mjs';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

const root = resolve(import.meta.dirname, '..');
const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const REQUEST_ID = 'A'.repeat(43);
const PLANTED = '/home/operator/planted-cwd-DO-NOT-SHOW/checkout';
const section = (items = [], extra = {}) => ({ items, truncated: false, dropped: 0, ...extra });
const view = (fields = {}) => ({
  state: 'ok',
  read_at: 1_787_394_600_000,
  head: { branch: 'agent/feature', id: 'aaaaaaaaaaaa', ahead: 2, behind: 0, against: 'remote_default_branch', untracked: 3, untracked_cut: false },
  branches: section([{ name: 'agent/feature', tip: 'aaaaaaaaaaaa', current: true }, { name: 'main', tip: 'bbbbbbbbbbbb', current: false }]),
  commits: section([{ id: 'aaaaaaaaaaaa', parents: ['bbbbbbbbbbbb'], at: 1_787_394_000_000, subject: 'agent two' }]),
  committed: section([{ path: 'src/new.rs', cut: false, kind: 'added' }]),
  changes: section([{ path: 'src/main.rs', cut: false, kind: 'modified', staged: true }]),
  ...fields,
});
const refused = (state) => view({
  state, head: null, branches: section(), commits: section(), committed: section(), changes: section(),
});
const body = (value) => JSON.stringify(value);

let server;
let GitPanel;
let AgentDetail;
let AgentDrawer;
let store;
let decodeGitView;
let getGitState;
let loadGit;
let needsRead;
let postGit;
let refusalText;
let resetGit;
let shellQuote;
let copyablePath;
before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-git-tab'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ GitPanel } = await server.ssrLoadModule('/src/renderer/git/GitPanel.tsx'));
  ({ AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx'));
  ({ AgentDrawer } = await server.ssrLoadModule('/src/renderer/AgentDrawer.tsx'));
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  // The module the panel imports, so that the state a test loads is the state the panel draws.
  ({ decodeGitView, getGitState, loadGit, needsRead, postGit, refusalText, resetGit, shellQuote, copyablePath }
    = await server.ssrLoadModule('/src/renderer/git/git.ts'));
});
after(async () => { await server?.close(); });
beforeEach(() => { resetGit(); store.resetFloorStore(); });

const row = { id: ADA, name: 'Ada', role: 'engineer', department: 'engineering', provider: 'claude', status: 'idle', mode: 'managed' };

/** A daemon that answers each request from `reply(url, init)`, and the requests it saw. */
function daemon(reply) {
  const seen = [];
  const deps = {
    fetch: async (url, init) => {
      seen.push({ url, init });
      const { status, text } = await reply(url, init);
      return { status, text: async () => text };
    },
    signedIn: () => true,
    random: () => {},
    refresh: () => { deps.refreshed += 1; },
    epoch: () => 1,
    session: () => 1,
    refreshed: 0,
  };
  return { deps, seen };
}
const ok = (value) => async () => ({ status: 200, text: body(value) });

test('the decoder accepts the daemon\'s view and keeps exactly its members', () => {
  const decoded = decodeGitView(body(view()));
  assert.deepEqual(decoded, view());
  assert.equal(decodeGitView(body(refused('busy'))).state, 'busy');
  for (const state of ['agent_unknown', 'not_a_checkout_root', 'gitfile_unsupported', 'unsupported_storage',
    'unsupported_platform', 'git_unavailable', 'timed_out', 'unreadable', 'worktree_mismatch', 'worktree_released',
    'worktree_removed']) {
    assert.equal(decodeGitView(body(refused(state))).state, state);
  }
  assert.equal(decodeGitView(body(view({ head: { ...view().head, branch: null, id: null, ahead: null, behind: null } }))).head.branch, null);
  assert.equal(decodeGitView(body(view({ head: { ...view().head, against: 'worktree_start' } }))).head.against, 'worktree_start');
});

test('the decoder refuses what the daemon never sends', () => {
  const bad = (what, value) => assert.equal(decodeGitView(typeof value === 'string' ? value : body(value)), null, what);
  bad('an unknown member', { ...view(), extra: 1 });
  bad('a missing member', (({ changes, ...rest }) => rest)(view()));
  bad('an unknown state', view({ state: 'fine' }));
  bad('an unknown member of the head', view({ head: { ...view().head, remote: 'origin' } }));
  bad('a head without its comparison', view({ head: (({ against, ...rest }) => rest)(view().head) }));
  bad('an unknown comparison', view({ head: { ...view().head, against: 'origin/main' } }));
  bad('a refusal that says what it compared with', { ...refused('worktree_released'), head: { ...view().head, against: 'worktree_start' } });
  bad('an unknown member of an entry', view({ changes: section([{ path: 'a', cut: false, kind: 'added', staged: false, author: 'x' }]) }));
  bad('an unknown kind', view({ committed: section([{ path: 'a', cut: false, kind: 'copied' }]) }));
  bad('a refusal that carries a head', { ...refused('busy'), head: view().head });
  bad('a refusal that carries entries', { ...refused('busy'), changes: section([{ path: 'a', cut: false, kind: 'added', staged: false }]) });
  bad('ok without a head', view({ head: null }));
  bad('an absolute path', view({ changes: section([{ path: `${PLANTED}/x.txt`, cut: false, kind: 'added', staged: false }]) }));
  bad('a path that climbs out', view({ committed: section([{ path: 'a/../../b', cut: false, kind: 'added' }]) }));
  bad('an empty path component', view({ committed: section([{ path: 'a//b', cut: false, kind: 'added' }]) }));
  bad('a right-to-left override in a path', view({ committed: section([{ path: 'x‮gpj.exe', cut: false, kind: 'added' }]) }));
  bad('a newline in a branch', view({ branches: section([{ name: 'a\nb', tip: 'aaaaaaaaaaaa', current: false }]) }));
  bad('a zero-width joiner in a subject', view({ commits: section([{ id: 'aaaaaaaaaaaa', parents: [], at: 1, subject: 'a‍b' }]) }));
  bad('a line separator in a subject', view({ commits: section([{ id: 'aaaaaaaaaaaa', parents: [], at: 1, subject: 'a b' }]) }));
  bad('a long subject', view({ commits: section([{ id: 'aaaaaaaaaaaa', parents: [], at: 1, subject: 'x'.repeat(121) }]) }));
  bad('a long branch', view({ branches: section([{ name: 'x'.repeat(101), tip: 'aaaaaaaaaaaa', current: false }]) }));
  bad('a long path', view({ committed: section([{ path: 'x'.repeat(201), cut: false, kind: 'added' }]) }));
  bad('a short id', view({ branches: section([{ name: 'a', tip: 'abc', current: false }]) }));
  bad('an upper-case id', view({ branches: section([{ name: 'a', tip: 'AAAAAAAAAAAA', current: false }]) }));
  bad('five parents', view({ commits: section([{ id: 'aaaaaaaaaaaa', parents: Array(5).fill('bbbbbbbbbbbb'), at: 1, subject: 's' }]) }));
  bad('thirty-one branches', view({ branches: section(Array.from({ length: 31 }, (_, n) => ({ name: `b${n}`, tip: 'aaaaaaaaaaaa', current: false }))) }));
  bad('twenty-one commits', view({ commits: section(Array.from({ length: 21 }, () => ({ id: 'aaaaaaaaaaaa', parents: [], at: 1, subject: 's' }))) }));
  bad('a section over its byte budget', view({ committed: section(Array.from({ length: 900 }, (_, n) => ({ path: `${'d'.repeat(30)}/file-${n}.txt`, cut: false, kind: 'added' }))) }));
  bad('a negative count', view({ head: { ...view().head, untracked: -1 } }));
  bad('a fractional count', view({ head: { ...view().head, ahead: 1.5 } }));
  bad('a reply over 64 KiB', `${body(view())}${' '.repeat(64 * 1024)}`);
  bad('not JSON', 'nope');
  bad('an array', '[]');
});

test('a quoted path is one shell word that runs nothing, whatever it holds', () => {
  assert.equal(shellQuote('src/main.rs'), "'src/main.rs'");
  assert.equal(shellQuote("it's"), "'it'\\''s'");
  for (const path of ["'; rm -rf ~; '", 'a b', '$(touch /tmp/pwned)', '`id`', '-rf', 'new\nline', '"x"', 'back\\slash', "''", '*']) {
    const printed = execFileSync('/bin/sh', ['-c', `printf %s ${shellQuote(path)}`], { encoding: 'utf8' });
    assert.equal(printed, path, JSON.stringify(path));
  }
});

test('Copy path is explicitly relative, so no option or editor command can claim it', () => {
  assert.equal(copyablePath('src/main.rs'), "'./src/main.rs'");
  assert.equal(copyablePath('+!touch pwned'), "'./+!touch pwned'");
  assert.equal(copyablePath('-rf'), "'./-rf'");
  assert.equal(copyablePath("it's"), "'./it'\\''s'");
  for (const path of ['+!touch pwned', '-rf', '--help', "'; rm -rf ~; '"]) {
    const printed = execFileSync('/bin/sh', ['-c', `printf %s ${copyablePath(path)}`], { encoding: 'utf8' });
    assert.equal(printed, `./${path}`);
    assert.ok(!/^[-+!]/.test(printed), 'the first character is never one an option or an editor reads');
  }
});

test('the one request is the agent and nothing else, with nothing else', async () => {
  const { deps, seen } = daemon(ok(view()));
  const outcome = await postGit(ADA, deps.fetch);
  assert.equal(outcome.kind, 'view');
  assert.equal(seen.length, 1);
  assert.equal(seen[0].url, '/floor/git');
  assert.equal(seen[0].init.method, 'POST');
  assert.equal(seen[0].init.body, `{"agent_id":"${ADA}"}`);
  assert.deepEqual(seen[0].init.headers, { 'Content-Type': 'application/json' });
  assert.equal(seen[0].init.credentials, 'same-origin');
  assert.equal(seen[0].init.cache, 'no-store');
});

test('every reply the route can give is one finite outcome', async () => {
  const outcome = async (status, text) => (await postGit(ADA, daemon(async () => ({ status, text })).deps.fetch)).kind;
  assert.equal(await outcome(200, body(view())), 'view');
  assert.equal(await outcome(200, body({ ...view(), extra: 1 })), 'unknown');
  assert.equal(await outcome(200, ''), 'unknown');
  assert.equal(await outcome(403, ''), 'forbidden');
  assert.equal(await outcome(400, ''), 'rejected');
  assert.equal(await outcome(413, ''), 'rejected');
  assert.equal(await outcome(503, ''), 'unavailable');
  assert.equal(await outcome(500, ''), 'unknown');
  const broken = await postGit(ADA, async () => { throw new Error('offline'); });
  assert.equal(broken.kind, 'unavailable');
});

test('loading keeps the last view on screen and drops an answer to an earlier request', async () => {
  let release;
  const first = new Promise((go) => { release = go; });
  let calls = 0;
  const { deps } = daemon(async () => {
    calls += 1;
    if (calls === 1) await first;
    return { status: 200, text: body(view({ read_at: calls })) };
  });
  const slow = loadGit(ADA, deps);
  assert.equal(getGitState().phase, 'loading');
  await loadGit(ADA, deps);
  assert.equal(getGitState().view.read_at, 2);
  release();
  await slow;
  assert.equal(getGitState().view.read_at, 2, 'the late first answer did not replace the second');
  assert.equal(getGitState().phase, 'ready');
  const again = loadGit(ADA, daemon(async () => ({ status: 200, text: body(refused('busy')) })).deps);
  assert.equal(getGitState().view.read_at, 2, 'the last view stays while the next is read');
  await again;
  assert.equal(getGitState().view.read_at, 2, 'a busy answer does not wipe the reading on screen');
  assert.equal(getGitState().busy, true);
  assert.match(render(), /Try again in a few seconds/);
  assert.match(render(), /agent\/feature/);
  await loadGit(ADA, daemon(ok(view({ read_at: 9 }))).deps);
  assert.equal(getGitState().busy, false);
  await loadGit(ADA, daemon(async () => ({ status: 200, text: body(refused('unreadable')) })).deps);
  assert.equal(getGitState().view.state, 'unreadable', 'any other refusal does replace it');
});

test('a busy answer with nothing on screen is the refusal itself', async () => {
  await loadGit(ADA, daemon(async () => ({ status: 200, text: body(refused('busy')) })).deps);
  assert.equal(getGitState().view.state, 'busy');
  assert.equal(getGitState().busy, false);
  assert.match(render(), /Try again in a few seconds/);
});

test('a 403 asks for a fresh snapshot and a missing session sends nothing', async () => {
  const { deps, seen } = daemon(async () => ({ status: 403, text: '' }));
  await loadGit(ADA, deps);
  assert.equal(getGitState().phase, 'failed');
  assert.equal(getGitState().failure.kind, 'forbidden');
  assert.equal(deps.refreshed, 1);
  const none = daemon(ok(view()));
  none.deps.signedIn = () => false;
  await loadGit(ADA, none.deps);
  assert.equal(none.seen.length, 0);
  assert.match(refusalText(null, getGitState().failure), /no Floor session/);
  assert.equal(seen.length, 1);
});

test('showing the tab reads once for an agent, and a later look reads nothing', async () => {
  assert.equal(needsRead(getGitState(), ADA, false), false, 'a hidden tab reads nothing');
  assert.equal(needsRead(getGitState(), ADA, true), true);
  await loadGit(ADA, daemon(ok(view())).deps);
  assert.equal(needsRead(getGitState(), ADA, true), false, 'the tab was shown before');
  assert.equal(needsRead(getGitState(), '01ARZ3NDEKTSV4RRFFQ69G5FAV', true), true, 'another agent reads');
});

const render = (props = {}) => hooks(renderToStaticMarkup(createElement(GitPanel, { row, active: false, ...props })));

test('each refusal is a sentence and shows no repository data', async () => {
  for (const [state, pattern] of [
    ['not_a_checkout_root', /does not work at the root of a Git checkout/],
    ['gitfile_unsupported', /link file that Dudes did not create/],
    ['worktree_mismatch', /no longer matches the one Dudes created/],
    ['worktree_released', /retired, and the Git tab does not read the worktree it left behind/],
    ['worktree_removed', /retired, and its worktree has been removed/],
    ['unsupported_storage', /storage the Git tab does not read/],
    ['unsupported_platform', /this computer/],
    ['git_unavailable', /no usable git/],
    ['busy', /Try again in a few seconds/],
    ['timed_out', /did not finish in time/],
    ['unreadable', /could not be read as a repository/],
    ['agent_unknown', /does not know this agent/],
  ]) {
    resetGit();
    await loadGit(ADA, daemon(ok(refused(state))).deps);
    const html = render();
    assert.match(html, pattern, state);
    assert.doesNotMatch(html, /Local branches|Uncommitted changes/, state);
  }
});

test('the tab shows the head, branches, commits and files, and says what it will not', async () => {
  await loadGit(ADA, daemon(ok(view())).deps);
  const html = render();
  assert.match(html, /agent\/feature/);
  assert.match(html, /2 commits ahead of and 0 commits behind the remote default branch/);
  assert.match(html, /<dt>Untracked files<\/dt><dd>3<span class="git-note"> Their names are never shown\.<\/span><\/dd>/);
  assert.match(html, /agent two/);
  assert.match(html, /src\/new\.rs/);
  assert.match(html, /Modified, staged/);
  assert.match(html, /never untracked files, authors or where the checkout is/);
  assert.equal((html.match(/class="git-copy"/g) ?? []).length, 2);
  assert.match(html, /aria-describedby="git-committed-0"/);
  assert.match(html, /id="git-committed-0"/);
});

test('a worktree read is measured from the commit the worktree started from, and says so', async () => {
  const worktree = (head) => view({ head: { ...view().head, branch: 'dudes/0123456789ab', against: 'worktree_start', ...head } });
  await loadGit(ADA, daemon(ok(worktree({ ahead: 1, behind: 0 }))).deps);
  let html = render();
  assert.match(html, /1 commit ahead of and 0 commits behind the commit this worktree started from\./);
  assert.match(html, /<h4 id="git-commits-title">Commits since this worktree started<\/h4>/);
  assert.match(html, /<h4 id="git-committed-title">Files committed since this worktree started<\/h4>/);
  assert.doesNotMatch(html, /remote default branch|leaving the default branch/);
  resetGit();
  await loadGit(ADA, daemon(ok(worktree({ ahead: null, behind: null }))).deps);
  html = render();
  assert.match(html, /The commit this worktree started from is no longer in the repository\./);
  assert.doesNotMatch(html, /No remote default branch/);
  resetGit();
  await loadGit(ADA, daemon(ok(view())).deps);
  html = render();
  assert.match(html, /<h4 id="git-commits-title">Commits not on the remote default branch<\/h4>/);
  assert.doesNotMatch(html, /this worktree started/);
});

test('a cut section and a dropped entry are said, and a cut path cannot be copied', async () => {
  await loadGit(ADA, daemon(ok(view({
    committed: section([{ path: 'long/pa', cut: true, kind: 'added' }], { truncated: true, dropped: 2 }),
    head: { ...view().head, untracked: 9, untracked_cut: true, ahead: null, behind: null },
  }))).deps);
  const html = render();
  assert.match(html, /Showing the first 1 entry; the rest are not shown\./);
  assert.match(html, /2 entries were left out because their text held control or formatting characters\./);
  assert.match(html, /At least 9/);
  assert.match(html, /No remote default branch is recorded here/);
  assert.match(html, /long\/pa…/);
  assert.match(html, /<button type="button" aria-describedby="git-committed-0" disabled="" title="The path is shortened[^"]*" class="git-copy">/);
});

test('no repository string reaches an attribute, a title or an ARIA label', async () => {
  const SECRET = 'repo-string-SENTINEL';
  await loadGit(ADA, daemon(ok(view({
    head: { ...view().head, branch: `${SECRET}-branch` },
    branches: section([{ name: `${SECRET}-name`, tip: 'aaaaaaaaaaaa', current: true }]),
    commits: section([{ id: 'aaaaaaaaaaaa', parents: [], at: 1, subject: `${SECRET}-subject` }]),
    committed: section([{ path: `${SECRET}/committed.rs`, cut: false, kind: 'added' }]),
    changes: section([{ path: `${SECRET}/changed.rs`, cut: false, kind: 'modified', staged: false }]),
  }))).deps);
  const html = render();
  assert.equal((html.match(new RegExp(SECRET, 'g')) ?? []).length, 5, 'each string is drawn once, as text');
  const attributes = [...html.matchAll(/\s[a-z-]+="([^"]*)"/g)].map((match) => match[1]);
  assert.ok(attributes.length > 5);
  assert.deepEqual(attributes.filter((value) => value.includes(SECRET)), []);
  assert.doesNotMatch(html, /data-/);
  assert.match(html, /<time[^>]*title="[^"]*1970[^"]*">[^<]*ago<\/time>/);
  assert.ok(!html.includes('<script') && !html.includes('<img'), 'text is never markup');
});

test('markup in a string is drawn as text', async () => {
  await loadGit(ADA, daemon(ok(view({
    commits: section([{ id: 'aaaaaaaaaaaa', parents: [], at: 1, subject: '<img src=x onerror=alert(1)>' }]),
  }))).deps);
  const html = render();
  assert.ok(!html.includes('<img'));
  assert.match(html, /&lt;img src=x onerror=alert\(1\)&gt;/);
});

test('the planted working directory is absent from the tab, and an absolute path is no reply at all', async () => {
  await loadGit(ADA, daemon(ok(view())).deps);
  assert.ok(!render().includes('planted-cwd'));
  resetGit();
  await loadGit(ADA, daemon(ok(view({
    committed: section([{ path: `${PLANTED}/x.rs`, cut: false, kind: 'added' }]),
    branches: section([{ name: PLANTED.slice(1), tip: 'aaaaaaaaaaaa', current: false }]),
  }))).deps);
  assert.equal(getGitState().phase, 'failed');
  assert.equal(getGitState().view, null);
  const html = render();
  assert.ok(!html.includes('planted-cwd'), 'a reply that names the checkout is dropped whole');
  assert.match(html, /reply this page could not read/);
});

test('the tab exists only when the daemon sent the sibling', () => {
  const tabs = (html) => [...html.matchAll(/role="tab" class="agent-detail-tab"[^>]*>([^<]+)<\/button>/g)].map((match) => match[1]);
  const detail = (git) => renderToStaticMarkup(createElement(AgentDetail, { row, git }));
  assert.deepEqual(tabs(detail(null)), ['Overview']);
  assert.deepEqual(tabs(detail(createElement('p', null, 'Git panel'))), ['Overview', 'Git']);
  const drawer = (git) => renderToStaticMarkup(createElement(AgentDrawer, {
    floor: { ...store.getFloorState(), phase: 'live', stale: false, agents: [row], git },
    row, selectedId: ADA, onClose() {}, leaveFocusTo: 'crew-toggle',
  }));
  assert.ok(!tabs(drawer(false)).includes('Git'));
  assert.ok(tabs(drawer(true)).includes('Git'));
  assert.equal(getGitState().phase, 'idle', 'drawing the tab reads nothing');
});

test('a snapshot carries the sibling only as an empty object', () => {
  const frame = (extra) => {
    const epoch = store.beginFloorEpoch();
    store.noteTransport('open');
    store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot',
      projection: { lsn: 10, agents: [row], cards: [] }, ...extra }), epoch);
    return store.getFloorState().git;
  };
  assert.equal(frame({}), false, 'absent is off');
  assert.equal(frame({ git: {} }), true);
  assert.equal(frame({ git: { enabled: true } }), false, 'anything but the empty object is off');
  assert.equal(frame({ git: true }), false);
  assert.equal(frame({ git: [] }), false);
  assert.equal(frame({ git: {} }), true);
  store.noteTransport('close');
  assert.equal(store.getFloorState().git, false, 'a closed stream forgets it');
});
