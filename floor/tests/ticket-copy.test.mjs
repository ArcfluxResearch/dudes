// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Where a ticket's agent works: the copy line on the ticket page, the branch chip on a Board card,
// and the earlier-round line. None shows a folder path.

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before, beforeEach } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

import { ADA, GRACE, agent, subscribe, ticketCard } from './helpers/tickets.mjs';

const root = resolve(import.meta.dirname, '..');
const BRANCH = 'dudes/0123456789ab';
const OTHER = 'dudes/ba9876543210';

let server;
let store;
let copy;
let TicketCopy;
let BranchChip;

before(async () => {
  server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-ticket-copy'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
  copy = await server.ssrLoadModule('/src/renderer/ticket/copy.ts');
  ({ TicketCopy, BranchChip } = await server.ssrLoadModule('/src/renderer/ticket/TicketCopy.tsx'));
});
after(async () => { await server?.close(); });
beforeEach(() => { store.resetFloorStore(); });

const own = (state, branch = BRANCH) => ({ worktree: { branch, state } });
const show = (agents, status = 'review', ticket = {}) => {
  subscribe(store, { agents, cards: [ticketCard(1, status, { claimed_by: ADA }, ticket)], git: true, diff: true });
  const floor = store.getFloorState();
  return { floor, card: floor.cards[0] };
};
const page = ({ floor, card }, history = null, extra = {}) => renderToStaticMarkup(createElement(TicketCopy,
  { floor, card, history, showChanges: true, onOpenGit() {}, ...extra }));
const started = (agent) => ({ events: [{ kind: 'started', agent, round: 1, lsn: '1', at: 1, by: 'daemon' }], omitted: 0 });

test('a ticket in an own copy in use shows its branch, the state and both links', () => {
  const html = page(show([agent(ADA, 'Ada', own('active'))]));
  assert.match(html, new RegExp(`data-ticket-branch[^>]*>${BRANCH}<`));
  assert.match(html, /data-ticket-copy-state="active"[^>]*>In use</);
  assert.match(html, /See this copy’s changes/);
  assert.match(html, /Open the agent’s Git view/);
});

test('a released copy says so', () => {
  const html = page(show([agent(ADA, 'Ada', own('released'))], 'done'));
  assert.match(html, /data-ticket-copy-state="released"[^>]*>Released/);
  assert.doesNotMatch(html, /In use/);
});

test('an agent in the project folder gets one plain line and no links', () => {
  const html = page(show([agent(ADA, 'Ada')]));
  assert.match(html, /This agent works in the project folder, not in a copy of its own\./);
  assert.doesNotMatch(html, /Branch|button/);
});

test('the Git link needs the changed-files switch and a handler', () => {
  const shown = show([agent(ADA, 'Ada', own('active'))]);
  assert.doesNotMatch(page(shown, null, { onOpenGit: undefined }), /Git view/);
  assert.doesNotMatch(page({ ...shown, floor: { ...shown.floor, git: false } }), /Git view/);
  assert.doesNotMatch(page(shown, null, { showChanges: false }), /See this copy/);
});

test('no folder path appears on the line', () => {
  const html = page(show([agent(ADA, 'Ada', { ...own('active'), cwd: '/home/ada/secret', path: '/var/copy' })]));
  const text = html.replace(/<[^>]*>/g, ' ');
  assert.doesNotMatch(text, /\/home|\/var|secret|\\/);
});

test('a reassigned ticket says the earlier round stays in the earlier agent’s copy', () => {
  const agents = [agent(ADA, 'Ada', own('active')), agent(GRACE, 'Grace', own('released', OTHER))];
  const shown = show(agents);
  assert.match(page(shown, started(GRACE)), /An earlier round was worked by Grace\. That work stays in Grace’s copy\./);
  assert.doesNotMatch(page(shown, started(ADA)), /earlier round/);
  assert.doesNotMatch(page(shown, null), /earlier round/);
  const sharing = show([agent(ADA, 'Ada', own('active')), agent(GRACE, 'Grace')]);
  assert.equal(copy.earlierRoundLine(sharing.floor, sharing.card, started(GRACE)), null);
});

test('a Doing or Review card carries the branch chip, other cards and shared folders do not', () => {
  const chip = (status, agents) => {
    const shown = show(agents, status);
    return renderToStaticMarkup(createElement(BranchChip, shown));
  };
  const mine = [agent(ADA, 'Ada', own('active'))];
  assert.match(chip('doing', mine), new RegExp(`ticket-branch[^>]*>${BRANCH}<`));
  assert.match(chip('review', mine), /ticket-branch/);
  assert.equal(chip('todo', mine), '');
  assert.equal(chip('done', mine), '');
  assert.equal(chip('doing', [agent(ADA, 'Ada')]), '');
  assert.equal(chip('doing', [agent(ADA, 'Ada', own('removed'))]), '');
});
