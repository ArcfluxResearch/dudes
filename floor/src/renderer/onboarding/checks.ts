// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The completion checks each enrolled workspace offers, as the daemon states
// them, and what a refused run of one says.
//
// They ride one optional `checks` field beside `projection` on a
// `floor.snapshot`, present while Hire or goal change is on. Hire and goal
// change name a check by its id and revision; the daemon matches both again
// and runs the check once before it appends anything. So, like the onboarding
// catalog, this list is display metadata, never authority, and this reader
// fails closed: anything absent, malformed or over the bound is "no checks
// known", never the entries that happen to parse.

import { checkIdentityOf } from '../goal/completionCheck.ts';

export interface EnrolledCheck {
  readonly id: string;
  readonly label: string;
  /** A content digest the daemon matches the chosen check against. */
  readonly revision: string;
  /** The operator declared that this check may already pass before any work. */
  readonly mayAlreadyPass: boolean;
}

export interface WorkspaceChecks {
  readonly workspaceId: string;
  /** At least one, in the daemon's order: the first is the workspace's own check. */
  readonly checks: readonly EnrolledCheck[];
}

export interface CheckCatalog {
  readonly workspaces: readonly WorkspaceChecks[];
}

const MAX_SERIALIZED_BYTES = 16_384;
const WORKSPACE_ID = /^[A-Za-z0-9_-]{1,128}$/;
const CATALOG_KEYS = Object.freeze(['workspaces']);
const WORKSPACE_KEYS = Object.freeze(['workspace_id', 'checks']);
const CHECK_KEYS = Object.freeze(['id', 'label', 'revision', 'may_already_pass']);
const UTF8 = new TextEncoder();

function exactly(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const own = Object.keys(value);
  return own.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function checkList(value: unknown): readonly EnrolledCheck[] | null {
  if (!Array.isArray(value) || value.length === 0) return null;
  const checks: EnrolledCheck[] = [];
  for (const entry of value) {
    if (!exactly(entry, CHECK_KEYS) || typeof entry.may_already_pass !== 'boolean') return null;
    const identity = checkIdentityOf(entry);
    if (identity === null || checks.some((check) => check.id === identity.id)) return null;
    checks.push(Object.freeze({ id: identity.id, label: identity.label, revision: identity.revision,
      mayAlreadyPass: entry.may_already_pass }));
  }
  return Object.freeze(checks);
}

/**
 * Read the optional `checks` sibling of one snapshot.
 *
 * @returns the list, or `null` for absent, malformed or over-bound data. It
 * never throws: an unreadable list must leave the stream and the floor alone.
 */
export function decodeCheckCatalog(value: unknown): CheckCatalog | null {
  if (!exactly(value, CATALOG_KEYS) || !Array.isArray(value.workspaces)) return null;
  if (UTF8.encode(JSON.stringify(value)).length > MAX_SERIALIZED_BYTES) return null;
  const workspaces: WorkspaceChecks[] = [];
  for (const entry of value.workspaces) {
    if (!exactly(entry, WORKSPACE_KEYS)) return null;
    const id = entry.workspace_id;
    const previous = workspaces.at(-1)?.workspaceId;
    if (typeof id !== 'string' || !WORKSPACE_ID.test(id) || (previous !== undefined && !(id > previous))) {
      return null;
    }
    const checks = checkList(entry.checks);
    if (checks === null) return null;
    workspaces.push(Object.freeze({ workspaceId: id, checks }));
  }
  return Object.freeze({ workspaces: Object.freeze(workspaces) });
}

/** The checks one workspace offers, or `null` when the list has no entry for it. */
export function checksFor(catalog: CheckCatalog | null, workspaceId: string): readonly EnrolledCheck[] | null {
  return catalog?.workspaces.find((entry) => entry.workspaceId === workspaceId)?.checks ?? null;
}

/** A check's label as a choice shows it. */
export function choiceLabel(check: EnrolledCheck): string {
  return check.mayAlreadyPass ? `${check.label} (may already pass)` : check.label;
}

/** Why the daemon ran no check, or what the one run it made found, refused before any effect. */
export const CHECK_REFUSALS = Object.freeze(
  ['check_running', 'check_limit', 'check_cannot_run', 'check_already_passes', 'check_did_not_finish'] as const);

export type CheckRefusal = (typeof CHECK_REFUSALS)[number];

export function isCheckRefusal(value: unknown): value is CheckRefusal {
  return (CHECK_REFUSALS as readonly unknown[]).includes(value);
}

/**
 * What one refusal says. `label` names the check, or is `null` for a check
 * recorded without a name; `route` says what did not happen.
 */
export function checkRefusalText(reason: CheckRefusal, label: string | null, route: 'hire' | 'goal'): string {
  const check = label === null ? 'This agent’s check' : `“${label}”`;
  const nothing = route === 'hire' ? 'was hired' : 'changed';
  switch (reason) {
    case 'check_already_passes':
      return `${check} already passes in this project, so it cannot show when this goal is done. Nothing ${nothing}. `
        + 'Choose another check, or ask the operator to add one for this goal with dudes project check add.';
    case 'check_did_not_finish':
      return `${check} did not finish, so it cannot show whether it already passes. Nothing ${nothing}. `
        + 'Try again, or ask the operator to raise its timeout.';
    case 'check_cannot_run':
      return `${check} could not run, so nothing ${nothing}. The daemon log says why.`;
    case 'check_running':
      return 'A check is still running for this Floor session. Try again when it ends.';
    case 'check_limit':
      return 'This Floor session has run its 32 checks. Open a new Floor session from a terminal (dudes floor open).';
  }
}
