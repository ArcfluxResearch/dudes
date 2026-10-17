// SPDX-License-Identifier: MIT OR Apache-2.0

export const WORKTREE_STATES = ['pending', 'active', 'released', 'removed'] as const;
export type WorktreeState = typeof WORKTREE_STATES[number];

/** An agent's own git worktree as the daemon reports it. Never a path. */
export interface Worktree {
  readonly branch: string;
  readonly state: WorktreeState;
  readonly from?: string;
}

const BRANCH = /^dudes\/[0-9a-f]{12}$/;
const LEASE = /^[0-9a-f]{32}$/;

export function decodeWorktree(value: unknown): Worktree | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  const source = value as Record<string, unknown>;
  const state = WORKTREE_STATES.find((name) => name === source.state);
  if (typeof source.branch !== 'string' || !BRANCH.test(source.branch) || state === undefined
    || (source.from !== undefined && (typeof source.from !== 'string' || !LEASE.test(source.from)))) return null;
  return Object.freeze({ branch: source.branch, state,
    ...(source.from === undefined ? {} : { from: source.from as string }) });
}

const STATE_TEXT: Readonly<Record<WorktreeState, string>> = Object.freeze({
  pending: 'being created',
  active: 'in use',
  released: 'released, not yet removed',
  removed: 'removed',
});

export function worktreeStateText(state: WorktreeState): string {
  return STATE_TEXT[state];
}

export function worktreeText(worktree: Worktree): string {
  return `Branch ${worktree.branch}, ${STATE_TEXT[worktree.state]}`;
}

/** Another live agent records this agent's working directory too. A hint, not a guard. */
export function sharesCheckout(row: Readonly<Record<string, unknown>>): boolean {
  return row.shares_checkout === true;
}

/** What one Crew row says about where its agent writes: its own worktree, a directory others share, or neither. */
export interface CheckoutOwnership {
  readonly worktree: string | null;
  readonly shared: boolean;
}

export const SHARED_DIRECTORY_TEXT = 'Shared directory';

export function checkoutOf(row: Readonly<Record<string, unknown>>): CheckoutOwnership {
  const worktree = decodeWorktree(row.worktree);
  return { worktree: worktree === null ? null : `Worktree ${worktree.branch}, ${STATE_TEXT[worktree.state]}`,
    shared: sharesCheckout(row) };
}

/** One line over Crew, or null when no agent has a worktree in use or shares a directory. Released and removed trees are not counted. */
export function checkoutSummary(rows: readonly Readonly<Record<string, unknown>>[]): string | null {
  const own = rows.filter((row) => {
    const state = decodeWorktree(row.worktree)?.state;
    return state === 'pending' || state === 'active';
  }).length;
  const shared = rows.filter(sharesCheckout).length;
  const parts = [own === 0 ? '' : `${own} in own worktree`, shared === 0 ? '' : `${shared} share a directory`]
    .filter((part) => part !== '');
  return parts.length === 0 ? null : parts.join(' · ');
}
