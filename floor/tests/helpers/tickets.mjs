// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the ticket tests share: ids, a snapshot builder, and a daemon that answers each request the
// page sends from a function.

export const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
export const GRACE = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
export const CSRF = 'A'.repeat(43);
export const REVISION = 'a'.repeat(64);
export const SECOND_REVISION = 'b'.repeat(64);

export const cardId = (n) => `01JBQ8XK2M3N4P5Q6R7S8T${String(n).padStart(4, '0')}`;

export const CHECKS = Object.freeze({
  workspaces: [{
    workspace_id: 'service',
    checks: [
      { id: 'default', label: 'default passes', revision: REVISION, may_already_pass: true },
      { id: 'second', label: 'second passes', revision: SECOND_REVISION, may_already_pass: false },
    ],
  }],
});

export const TWO_PROJECTS = Object.freeze({
  workspaces: [
    CHECKS.workspaces[0],
    { workspace_id: 'website', checks: [{ id: 'site', label: 'site builds', revision: 'c'.repeat(64), may_already_pass: false }] },
  ],
});

/** A managed agent hired into the project `service`. */
export function agent(id = ADA, name = 'Ada', fields = {}) {
  return {
    id, name, role: 'engineer', department: 'engineering', provider: 'claude', status: 'idle', mode: 'managed',
    turns: 0, workspace: { id: 'service', label: 'Service' }, ...fields,
  };
}

/** A ticket card as a snapshot carries it: codes and counts, no note and no summary. */
export function ticketCard(n, status, fields = {}, ticket = {}) {
  return {
    id: cardId(n), title: `Ticket ${n}`, department: 'engineering', status, status_by: { kind: 'system' }, seq: 3,
    claimed_by: null, deps: [], ticket: { ...ticket }, ...fields,
  };
}

export function plainCard(n, status = 'todo') {
  return {
    id: cardId(n), title: `Plain ${n}`, department: 'engineering', status, status_by: { kind: 'peer', pid: 1 }, seq: 0,
    claimed_by: null, deps: [],
  };
}

/**
 * A fresh live subscription on the real store. `ticket: null` leaves the sibling out, as a daemon
 * with the board off does; `git` adds the `git` sibling.
 */
export function subscribe(store, { agents = [], cards = [], ticket = {}, checks = CHECKS, settings = null, git = false, lsn = 12 } = {}) {
  const epoch = store.beginFloorEpoch();
  store.noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn, agents, cards } };
  if (ticket !== null) frame.ticket = ticket;
  if (checks !== null) frame.checks = checks;
  if (settings !== null) frame.settings = settings;
  if (git) frame.git = {};
  store.applyServerFrame(JSON.stringify(frame), epoch);
  return epoch;
}

/** A daemon that answers each request from `reply(url, body)`, and the requests it saw. */
export function daemon(reply) {
  const seen = [];
  const deps = {
    fetch: async (url, init) => {
      const body = init.body === undefined ? undefined : JSON.parse(init.body);
      seen.push({ url, body, init });
      const { status, text } = await reply(url, body, seen.length);
      return { status, text: async () => text };
    },
    csrf: () => CSRF,
    random: () => {},
    refresh: () => { deps.refreshed += 1; },
    epoch: () => 1,
    session: () => 1,
    refreshed: 0,
  };
  return { deps, seen };
}

export const json = (status, value) => ({ status, text: JSON.stringify(value) });

/** The five `done` answers the daemon gives for an action on `card`. */
export const done = (state, card, extra = {}) => json(200, { state, card, lsn: '4096', seq: 4, ...extra });

export const refused = (reason) => json(409, { state: 'refused', reason });

/** A node tree's elements that satisfy `matches`, found without rendering. */
export function find(node, matches) {
  if (node === null || typeof node !== 'object') return [];
  if (Array.isArray(node)) return node.flatMap((child) => find(child, matches));
  return [...(matches(node) ? [node] : []), ...find(node.props?.children, matches)];
}

export const INSTRUCTION_WATERMARK = Object.freeze({ through_lsn: '9007199254740993', set_digest: 'd'.repeat(64) });

export function instructionReview(fields = {}) {
  return {
    watermark: INSTRUCTION_WATERMARK, pending_comparison: false,
    intervals: [{ kind: 'managed', agent: ADA, turn: cardId(40), attachment_lsn: null, issue_lsn: '10',
      baseline_lsn: '11', missing_start: false, card: cardId(1), goal: cardId(41), assignment_lsn: '9', seq_at_start: 2,
      finished_lsn: '9007199254740993', end_lsn: '13', sampled_at: 1_787_394_600_000, recovery: true }],
    changed_paths: [{ path: { encoding: 'utf8', value: '/project/AGENTS\n.md' }, display: '/project/AGENTS\\n.md', display_truncated: false,
      context: { kind: 'shared_project' } }],
    gaps: [{ reason: 'read_failed', context: { kind: 'shared_input', source: 'import' }, agent: ADA,
      path: { encoding: 'unix_bytes', hex: '2fff' }, display: 'unix:2fff', display_truncated: false, origin: 'observed', holds: true }],
    ...fields,
  };
}

export function instructionSummary(fields = {}) {
  return { watermark: INSTRUCTION_WATERMARK, pending_comparison: false, intervals: 1, changed_paths: 1, gaps: 1, ...fields };
}
