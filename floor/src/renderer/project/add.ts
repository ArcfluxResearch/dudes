// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Adding a project from a folder path. The page asks the daemon to look at the folder, shows what
// the daemon found, and adds exactly that on one click. The daemon decides what the folder is: the
// page sends back the folder and check it was shown, and the daemon refuses with `changed` if
// either has moved since.

import { currentFloorSession } from '../../session.ts';
import { floorFetch } from '../../transport.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { editDraft, mintAttemptId } from '../onboarding/hire.ts';

export const REFUSALS = ['not_found', 'not_a_folder', 'not_allowed', 'not_git', 'changed', 'busy', 'config_changed', 'full',
  'action_off'] as const;
export type Refusal = (typeof REFUSALS)[number];
export type NotHireable = 'ceiling_required' | 'check_not_runnable';

/** The daemon's check: `files_changed`, or the command it will run. */
export type Check = 'files_changed' | readonly string[];

export interface ProjectPreview {
  readonly folder: string;
  readonly already: { readonly projectId: string; readonly label: string } | null;
  readonly check: Check;
  readonly checkLabel: string;
  readonly reason: NotHireable | null;
}

export interface ProjectAdded {
  readonly projectId: string;
  readonly label: string;
  readonly checkLabel: string;
  readonly reason: NotHireable | null;
}

export type AddState =
  | { readonly kind: 'idle' }
  | { readonly kind: 'looking' }
  | { readonly kind: 'preview'; readonly preview: ProjectPreview; readonly attempt: string }
  | { readonly kind: 'adding'; readonly preview: ProjectPreview; readonly attempt: string }
  | { readonly kind: 'added'; readonly added: ProjectAdded }
  | { readonly kind: 'refused'; readonly reason: Refusal }
  | { readonly kind: 'failed'; readonly step: 'look' }
  | { readonly kind: 'unsure'; readonly preview: ProjectPreview; readonly attempt: string };

export interface AddDeps {
  fetch: typeof floorFetch;
  csrf(): string | null;
  random(bytes: Uint8Array<ArrayBuffer>): void;
  refresh(): void;
  wait(milliseconds: number): Promise<void>;
}

export const browserAddDeps: AddDeps = {
  fetch: floorFetch,
  csrf: () => currentFloorSession()?.csrf ?? null,
  random: (bytes) => { crypto.getRandomValues(bytes); },
  refresh: refreshFloorSnapshot,
  wait: (milliseconds) => new Promise((done) => { setTimeout(done, milliseconds); }),
};

const IDLE: AddState = Object.freeze({ kind: 'idle' });
let state: AddState = IDLE;
const listeners = new Set<() => void>();

export function getAddState(): AddState { return state; }

export function subscribeToAdd(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

function commit(next: AddState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

export function resetAdd(): void { commit(IDLE); }

/** What people paste: a path in quotes, or with a trailing slash. */
export function normalizeFolder(typed: string): string {
  let path = typed.trim();
  if (path.length >= 2 && (path[0] === '"' || path[0] === "'") && path.endsWith(path[0]!)) path = path.slice(1, -1).trim();
  return path.replace(/(?<=.)\/+$/, '');
}

const MOST_PENDING_RETRIES = 20;

function object(text: string): Record<string, unknown> | null {
  let body: unknown;
  try { body = JSON.parse(text); } catch { return null; }
  return typeof body === 'object' && body !== null && !Array.isArray(body) ? body as Record<string, unknown> : null;
}

function reasonOf(value: unknown): NotHireable | null | undefined {
  return value === null || value === 'ceiling_required' || value === 'check_not_runnable' ? value : undefined;
}

function checkOf(value: unknown): Check | null {
  if (value === 'files_changed') return value;
  return Array.isArray(value) && value.length > 0 && value.every((word) => typeof word === 'string')
    ? Object.freeze([...value]) as readonly string[] : null;
}

export type Decoded<T> = { readonly kind: 'ok'; readonly value: T } | { readonly kind: 'refused'; readonly reason: Refusal }
  | { readonly kind: 'pending' } | { readonly kind: 'failed' };

function refusal(status: number, fields: Record<string, unknown>): Decoded<never> | null {
  if (status === 409 && fields.state === 'refused' && (REFUSALS as readonly unknown[]).includes(fields.reason)) {
    return { kind: 'refused', reason: fields.reason as Refusal };
  }
  return null;
}

export function decodePreview(status: number, text: string): Decoded<ProjectPreview> {
  const fields = object(text);
  if (fields === null) return { kind: 'failed' };
  const refused = refusal(status, fields);
  if (refused !== null) return refused;
  const check = checkOf(fields.check);
  const reason = reasonOf(fields.reason);
  const already = fields.already;
  const alreadyOk = already === null || (typeof already === 'object' && already !== null
    && typeof (already as Record<string, unknown>).project_id === 'string'
    && typeof (already as Record<string, unknown>).label === 'string');
  if (status !== 200 || typeof fields.folder !== 'string' || check === null || typeof fields.check_label !== 'string'
    || reason === undefined || typeof fields.hireable !== 'boolean' || fields.hireable !== (reason === null) || !alreadyOk) {
    return { kind: 'failed' };
  }
  const found = already as { project_id: string; label: string } | null;
  return { kind: 'ok', value: Object.freeze({ folder: fields.folder, check, checkLabel: fields.check_label, reason,
    already: found === null ? null : Object.freeze({ projectId: found.project_id, label: found.label }) }) };
}

export function decodeAdded(status: number, text: string): Decoded<ProjectAdded> {
  const fields = object(text);
  if (fields === null) return { kind: 'failed' };
  if (status === 202 && fields.state === 'pending') return { kind: 'pending' };
  const refused = refusal(status, fields);
  if (refused !== null) return refused;
  const reason = reasonOf(fields.reason);
  if (status !== 200 || typeof fields.project_id !== 'string' || typeof fields.label !== 'string'
    || typeof fields.check_label !== 'string' || reason === undefined || typeof fields.hireable !== 'boolean'
    || fields.hireable !== (reason === null)) return { kind: 'failed' };
  return { kind: 'ok', value: Object.freeze({ projectId: fields.project_id, label: fields.label,
    checkLabel: fields.check_label, reason }) };
}

const PREVIEW_PATH = '/floor/project/preview';
const ADD_PATH = '/floor/project/add';

async function post(path: typeof PREVIEW_PATH | typeof ADD_PATH, body: unknown, deps: AddDeps): Promise<{ status: number; text: string } | null> {
  const csrf = deps.csrf();
  if (csrf === null) return null;
  try {
    const response = await deps.fetch(path, {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf },
      body: JSON.stringify(body),
    });
    return { status: response.status, text: await response.text() };
  } catch {
    return null;
  }
}

/** Ask the daemon what adding this folder would do. */
export async function lookAtFolder(typed: string, deps: AddDeps = browserAddDeps): Promise<void> {
  if (state.kind === 'looking' || state.kind === 'adding') return;
  const path = normalizeFolder(typed);
  if (path === '') return;
  commit({ kind: 'looking' });
  const answer = await post(PREVIEW_PATH, { path }, deps);
  const decoded = answer === null ? null : decodePreview(answer.status, answer.text);
  if (decoded === null || decoded.kind === 'failed' || decoded.kind === 'pending') return commit({ kind: 'failed', step: 'look' });
  if (decoded.kind === 'refused') return commit({ kind: 'refused', reason: decoded.reason });
  commit({ kind: 'preview', preview: decoded.value, attempt: mintAttemptId(deps.random) });
}

/** Add the folder the preview showed, with the check it showed. */
export async function addPreviewed(deps: AddDeps = browserAddDeps): Promise<void> {
  const current = state;
  if (current.kind !== 'preview' && current.kind !== 'unsure') return;
  const { preview, attempt } = current;
  commit({ kind: 'adding', preview, attempt });
  const body = { folder: preview.folder, check: preview.check, attempt_id: attempt };
  for (let tries = 0; tries <= MOST_PENDING_RETRIES; tries += 1) {
    const answer = await post(ADD_PATH, body, deps);
    const decoded = answer === null ? null : decodeAdded(answer.status, answer.text);
    if (decoded === null || decoded.kind === 'failed') return commit({ kind: 'unsure', preview, attempt });
    if (decoded.kind === 'refused') return commit({ kind: 'refused', reason: decoded.reason });
    if (decoded.kind === 'ok') {
      commit({ kind: 'added', added: decoded.value });
      if (decoded.value.reason === null) editDraft({ workspaceId: decoded.value.projectId });
      deps.refresh();
      return;
    }
    await deps.wait(500);
  }
  commit({ kind: 'unsure', preview, attempt });
}

/** The plain sentence for a refusal. None of them tells the person to open a terminal. */
export function refusalLine(reason: Refusal): string {
  switch (reason) {
    case 'not_found': return 'Dudes cannot find or open that folder. Check the path and try again.';
    case 'not_a_folder': return 'That path is a file, not a folder.';
    case 'not_allowed': return 'Dudes will not use that folder. Choose the folder of one project, not your home folder, '
      + 'a hidden folder or a folder Dudes keeps for itself.';
    case 'not_git': return 'This folder is not a git repository. Dudes needs one to show you what an agent changed.';
    case 'changed': return 'That folder changed since you looked at it. Look at it again.';
    case 'busy': return 'Dudes is busy applying another change. Try again in a moment.';
    case 'config_changed': return 'Other changes to the settings file are waiting to be loaded, so nothing was changed. '
      + 'Restart Dudes to load them, then add the project again.';
    case 'full': return 'Dudes already runs 32 projects, the most it can. Remove one first.';
    case 'action_off': return 'Adding projects from this page is turned off. Turn on Add projects in Settings.';
  }
}

/** What the daemon found, in words. */
export function previewLines(preview: ProjectPreview): readonly string[] {
  const lines: string[] = [];
  if (preview.already !== null) {
    lines.push(`${preview.folder} is already the project ${preview.already.label}.`);
    return lines;
  }
  lines.push(`Dudes found a git repository at ${preview.folder}.`);
  lines.push(preview.check === 'files_changed'
    ? 'No test command was found there. Dudes will count the work as done once an agent has edited or added a file, '
      + 'and you review the change.'
    : `Dudes will check an agent's work by running ${preview.checkLabel}.`);
  if (preview.reason === 'ceiling_required') {
    lines.push('Your account bills for each use, so set a spending limit for this project in Settings before you hire here.');
  }
  if (preview.reason === 'check_not_runnable') {
    lines.push('That check cannot run on this computer yet, so you cannot hire here until it can.');
  }
  return lines;
}

export function addedLine(added: ProjectAdded): string {
  if (added.reason === 'ceiling_required') {
    return `${added.label} is added. Set a spending limit for it in Settings before you hire here.`;
  }
  if (added.reason === 'check_not_runnable') return `${added.label} is added, but its check cannot run on this computer yet.`;
  return `${added.label} is added. You can hire an agent in it now.`;
}
