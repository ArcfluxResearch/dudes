// SPDX-License-Identifier: MIT OR Apache-2.0
//
// One Hire: three sections of choices, one request, one truthful answer.
//
// The page names an agent, gives it a role, and picks one workspace the operator
// enrolled and one engine the daemon offers for it. Everything else that
// decides how the agent runs — folder, command, ceilings, isolation, trust — is
// the daemon's configuration and never a field here. The daemon hires and
// writes the agent to its own journal; the page only asks, and then says what
// the answer proves.
//
// The hire carries no task and no check. The new agent is idle, and the Floor's
// first-task card offers to give it work.
//
// What the answer proves is narrow on purpose. Only `complete` is a success.
// `pending`, a 503 and a lost response prove nothing either way, so the page
// keeps the attempt and offers to ask again with it. A refusal the daemon made
// before any effect lets the operator edit and try again.
//
// The attempt lives here, in this module's memory, and nowhere else: not in
// the URL, not in browser storage, not in a log. It is minted once, by the
// Hire action, together with the exact request body, and every later Check
// status sends that same body byte for byte. A reload loses it — and then the
// page shows what the journal says is on the floor rather than guessing, and
// never sends a Hire by itself.

import { isAccentColour, isCastCharacter } from '../appearance.ts';
import type { AccentColour } from '../appearance.ts';
import type { RetiredAgentRow } from '../archive.ts';
import type { AgentRow } from '../scene/cast.ts';
import { rowFor } from '../setup/daemon.ts';
import { decodeWorkspace } from '../workspace.ts';

import { currentFloorSession } from '../../session.ts';
import { AGENT_ID, getFloorState } from '../floor/store.ts';
import type { FloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { HIRE_ENGINES, isHireEngine } from './catalog.ts';
import type { EnrolledWorkspace, HireEngine, OnboardingCatalog } from './catalog.ts';
import { floorFetch } from '../../transport.ts';

export type SectionKey = 'identity' | 'workspace' | 'engine';

/** The three sections, in order; one is shown at a time. */
export const SECTIONS: readonly { readonly key: SectionKey; readonly label: string; readonly hint: string }[] =
  Object.freeze([
    { key: 'identity', label: 'Who', hint: 'Name and look' },
    { key: 'workspace', label: 'Project', hint: 'Where they work' },
    { key: 'engine', label: 'Engine', hint: 'Claude, Codex or Pi' },
  ]);

/** What an agent is called when the person gives no role. */
export const DEFAULT_ROLE = 'engineer';

/** Field limits in UTF-8 bytes, measured after trimming ASCII whitespace. */
export const LIMITS = Object.freeze({ name: 128, role: 512 });

/** What to tell the person about a field's length: nothing until it is near the limit. */
export function lengthNote(value: string, limit: number): string | null {
  const used = utf8Bytes(trimAsciiSpace(value));
  if (used > limit) return 'This is too long. Shorten it.';
  return used >= limit * 0.9 ? 'This is almost too long.' : null;
}

export type FieldName = 'name' | 'role' | 'workspace_id' | 'engine' | 'model';

export type HireOffer = Pick<FloorState, 'onboarding' | 'agents' | 'daemon'>;

/** Whether the confirmed view says tickets are off, so nobody can give a new agent a task from the Floor. */
export function ticketsOff(floor: FloorState): boolean {
  return floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch && !floor.ticket;
}

export const TICKETS_OFF_AFTER = 'Tickets are off, so nobody can give this agent a task from the Floor yet. Turn on tickets to do that.';

/** The models the page suggests for an engine; any other name may be typed. */
export const MODEL_SUGGESTIONS: Readonly<Record<HireEngine, readonly string[]>> = Object.freeze({
  claude: Object.freeze(['opus', 'sonnet', 'haiku']),
  codex: Object.freeze([]),
  pi: Object.freeze([]),
});

/** The shape the daemon admits for a model name: 1 to 64 bytes, not starting with a dash. */
export const MODEL_SHAPE = /^[A-Za-z0-9._:/@[\]][A-Za-z0-9._:/@[\]-]{0,63}$/;

export const BAD_MODEL = 'A model name is 1 to 64 letters, digits, dots, dashes, underscores, colons, slashes, at signs and square brackets, and does not start with a dash.';
export const NO_PROJECT_LIST = 'Hire cannot list your projects right now. Reload the page, and check Settings if this stays.';

export interface HireDraft {
  readonly name: string;
  readonly role: string;
  readonly workspaceId: string;
  /** The engine chosen, which counts only while the chosen workspace offers it. */
  readonly engine?: HireEngine;
  /** The model every run of the agent names. Absent, or empty, is the engine's default. */
  readonly model?: string;
  /**
   * Whether the agent works in its own copy of the project. Absent until the person decides, so a
   * project another agent already works in can start with the box ticked and say why.
   */
  readonly ownCopy?: boolean;
  readonly character?: string;
  readonly accent?: AccentColour;
}

export interface FieldProblem {
  /** The field at fault, or `null` for a problem no field owns. */
  readonly field: FieldName | null;
  readonly section: SectionKey;
  readonly message: string;
}

/**
 * The request includes optional appearance choices and an enrolled workspace
 * revision. A Claude hire leaves `provider` out: the daemon refuses an explicit
 * `claude` as noncanonical.
 */
export interface HirePayload {
  readonly attempt_id: string;
  readonly workspace_id: string;
  readonly workspace_revision?: string;
  readonly name: string;
  readonly role: string;
  readonly provider?: Exclude<HireEngine, 'claude'>;
  /** Sent only when a model is chosen; absent means the engine's default. */
  readonly model?: string;
  /** Sent only when the agent should work in its own copy of the project. */
  readonly own_copy?: true;
  readonly character?: string;
  readonly accent?: AccentColour;
}

/** One attempt: its frozen payload and the serialized body every send reuses. */
export interface HireAttempt {
  readonly payload: HirePayload;
  readonly body: string;
}

/** What one answer proves. Only `complete` is a success. */
export type HireOutcome =
  | { readonly kind: 'complete'; readonly agent: string }
  /** 202: in flight or unresolved. No agent. */
  | { readonly kind: 'pending' }
  /** 503: durability could not be established. No agent. */
  | { readonly kind: 'unavailable' }
  /** A lost response, an unexpected status or an unreadable body. No agent. */
  | { readonly kind: 'unknown' }
  | { readonly kind: 'refused'; readonly note?: string }
  /** Empty 409: this attempt names a different request. Refused before any effect. */
  | { readonly kind: 'conflict' }
  /**
   * 409 refused `profile_changed`: a reload changed the workspace after this
   * page was offered it. Refused before any effect.
   */
  | { readonly kind: 'stale' }
  /** 400 or 413: the request itself was refused before any effect. */
  | { readonly kind: 'rejected'; readonly status: number };

export interface HireState {
  readonly draft: HireDraft;
  readonly section: SectionKey;
  readonly problem: FieldProblem | null;
  /** The one attempt in flight or awaiting an answer. Module memory only. */
  readonly attempt: HireAttempt | null;
  readonly sending: boolean;
  readonly outcome: HireOutcome | null;
  /**
   * The floor epoch when the page asked for a fresh snapshot after `outcome`.
   * A snapshot from a later epoch was taken after the daemon answered.
   */
  readonly askedAtEpoch: number;
  /** The AgentId already revealed on the floor for this outcome. */
  readonly revealed: string | null;
  readonly prefill: { readonly notes: Readonly<Partial<Record<SectionKey, string>>> } | null;
  /**
   * Whether any reply to this attempt left its effect unknown: `pending`, a 503
   * or a lost response. Once it has, a later refusal proves only that the later
   * request did nothing — the earlier one may already have hired.
   */
  readonly uncertain: boolean;
}

export interface HireResponse {
  readonly status: number;
  text(): Promise<string>;
}

export interface HireDeps {
  fetch(url: string, init: RequestInit): Promise<HireResponse>;
  /** Whether the page holds a sign-in. */
  signedIn(): boolean;
  /** Fill `bytes` with cryptographically random values. */
  random(bytes: Uint8Array<ArrayBuffer>): void;
  /** Ask the stream for a fresh snapshot of this session. */
  refresh(): void;
  /** The floor's current subscription epoch. */
  epoch(): number;
}

export const browserHireDeps: HireDeps = {
  fetch: floorFetch,
  signedIn: () => currentFloorSession() !== null,
  random: (bytes) => {
    crypto.getRandomValues(bytes);
  },
  refresh: () => {
    refreshFloorSnapshot();
  },
  epoch: () => getFloorState().epoch,
};

const EMPTY_DRAFT: HireDraft = Object.freeze({ name: '', role: '', workspaceId: '' });
const INITIAL: HireState = Object.freeze({
  draft: EMPTY_DRAFT, section: 'identity', problem: null, attempt: null,
  sending: false, outcome: null, askedAtEpoch: 0, revealed: null, uncertain: false,
  prefill: null,
});

/** The canonical attempt form: 32 bytes, unpadded base64url, no stray bits. */
export const ATTEMPT_ID = /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/;
const UTF8 = new TextEncoder();

let state: HireState = INITIAL;
const listeners = new Set<() => void>();

export function getHireState(): HireState {
  return state;
}

export function subscribeToHire(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** What a reload does: forget everything. For tests; the page gets a fresh module per load. */
export function resetHire(): void {
  state = INITIAL;
  listeners.clear();
}

function commit(next: HireState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

/** Trim only ASCII space, tab, CR and LF, as the daemon does; interior bytes stay. */
export function trimAsciiSpace(value: string): string {
  return value.replace(/^[ \t\r\n]+|[ \t\r\n]+$/g, '');
}

export function utf8Bytes(value: string): number {
  return UTF8.encode(value).length;
}

function bounded(label: string, value: string, limit: number, empty = `${label} is required.`): string | null {
  const bytes = utf8Bytes(trimAsciiSpace(value));
  if (bytes === 0) return empty;
  if (bytes > limit) return `${label} is too long. Shorten it.`;
  return null;
}

/** The enrolled workspace a draft names, if the catalog still offers it. */
export function chosenWorkspace(draft: HireDraft, catalog: OnboardingCatalog | null): EnrolledWorkspace | null {
  return catalog?.workspaces.find((space) => space.workspaceId === draft.workspaceId) ?? null;
}

/**
 * The engine this draft would hire: the one chosen, when the workspace offers
 * it, or else the workspace's only offered or found engine. `null` while there is no choice.
 */
export function chosenEngine(
  draft: HireDraft, workspace: EnrolledWorkspace | null, daemon: FloorState['daemon'] = null,
): HireEngine | null {
  if (workspace === null) return null;
  if (draft.engine !== undefined && workspace.engines.includes(draft.engine)) return draft.engine;
  if (workspace.engines.length === 1) return workspace.engines[0]!;
  const found = workspace.engines.filter((name) => rowFor(daemon, name)?.found === true);
  return found.length === 1 ? found[0]! : null;
}

/** The agents the projection shows working in the project `workspaceId`. */
export function agentsIn(agents: readonly AgentRow[], workspaceId: string): readonly AgentRow[] {
  return agents.filter((row) => decodeWorkspace(row.workspace)?.id === workspaceId);
}

/**
 * Whether this hire can work in its own copy of the project, and whether the box is ticked.
 *
 * - `untrusted`: the operator has not trusted the project, so there is no box, only the way to
 *   allow it.
 * - `engine`: the project is trusted but only Claude can work in a copy.
 * - `available`: the box shows. It starts ticked until the person decides, and carries a reason
 *   when another agent already works in the project's folder; their own choice wins after that.
 */
export type OwnCopy =
  | { readonly state: 'none' }
  | { readonly state: 'untrusted' }
  | { readonly state: 'engine' }
  | { readonly state: 'available'; readonly checked: boolean; readonly preselected: boolean };

export function ownCopyOffer(
  draft: HireDraft, workspace: EnrolledWorkspace | null, engine: HireEngine | null, sharing: number,
): OwnCopy {
  if (workspace === null || engine === null) return { state: 'none' };
  if (!workspace.trusted) return { state: 'untrusted' };
  if (engine !== 'claude') return { state: 'engine' };
  const preselected = draft.ownCopy === undefined && sharing > 0;
  return { state: 'available', checked: draft.ownCopy ?? true, preselected };
}

/** Every problem with a draft, in section order. */
export function problemsIn(draft: HireDraft, offer: HireOffer): readonly FieldProblem[] {
  const catalog = offer.onboarding;
  const problems: FieldProblem[] = [];
  const name = bounded('Name', draft.name, LIMITS.name);
  if (name !== null) problems.push({ field: 'name', section: 'identity', message: name });
  if (utf8Bytes(trimAsciiSpace(draft.role)) > LIMITS.role) {
    problems.push({ field: 'role', section: 'identity', message: 'The role is too long. Shorten it.' });
  }
  const workspace = chosenWorkspace(draft, catalog);
  if (catalog === null) {
    problems.push({ field: 'workspace_id', section: 'workspace',
      message: NO_PROJECT_LIST });
  } else if (draft.workspaceId === '') {
    problems.push({ field: 'workspace_id', section: 'workspace', message: 'Choose a project.' });
  } else if (workspace === null) {
    problems.push({ field: 'workspace_id', section: 'workspace',
      message: 'That project is no longer offered. Choose another.' });
  } else if (chosenEngine(draft, workspace, offer.daemon) === null) {
    problems.push({ field: 'engine', section: 'engine', message: 'Choose an engine this project offers.' });
  }
  const model = trimAsciiSpace(draft.model ?? '');
  if (model !== '' && !MODEL_SHAPE.test(model)) {
    problems.push({ field: 'model', section: 'engine', message: BAD_MODEL });
  }
  return problems;
}

/** A fresh attempt ID from 32 random bytes. */
export function mintAttemptId(random: HireDeps['random']): string {
  const bytes = new Uint8Array(32);
  random(bytes);
  let binary = '';
  for (const byte of bytes) binary += String.fromCharCode(byte);
  const id = btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  if (!ATTEMPT_ID.test(id)) throw new TypeError('attempt id is not canonical');
  return id;
}

export function editDraft(patch: Partial<HireDraft>): void {
  if ((patch.character !== undefined && patch.character !== '' && !isCastCharacter(patch.character))
    || (patch.accent !== undefined && !isAccentColour(patch.accent))) {
    throw new TypeError('unknown hire appearance');
  }
  // The payload is frozen with its attempt: while one is open, nothing edits it.
  if (state.attempt !== null) return;
  const next = { ...state.draft, ...patch };
  // A model chosen for one engine says nothing about another's.
  if (patch.engine !== undefined && patch.engine !== state.draft.engine && !('model' in patch)) delete next.model;
  // A choice about one project's copy says nothing about another's.
  if (patch.workspaceId !== undefined && patch.workspaceId !== state.draft.workspaceId && !('ownCopy' in patch)) {
    delete next.ownCopy;
  }
  commit({ ...state, draft: Object.freeze(next), problem: null });
}

export function prepareArchiveHire(row: RetiredAgentRow, floor: FloorState): boolean {
  if (floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return false;
  const settled = state.outcome?.kind === 'complete';
  if (state.sending || (state.attempt !== null
    && !(settled && freshSince(floor, state.askedAtEpoch) && floor.onboarding !== null))) return false;
  const matches = floor.onboarding?.workspaces.filter((space) => space.label === row.workspace_label) ?? [];
  const match = matches.length === 1 ? matches[0]! : null;
  const archived = isHireEngine(row.provider) ? row.provider : null;
  const offered = archived !== null && match !== null && match.engines.includes(archived);
  const missingEngine = archived !== null && rowFor(floor.daemon, archived)?.found === false;
  // The workspace is preselected only with the archived engine, so that no
  // other engine is ever substituted without the operator choosing it.
  const engine = offered && !missingEngine ? archived : null;
  const workspaceId = engine === null ? '' : match!.workspaceId;
  const workspace = row.workspace_label === undefined
    ? 'The archived project was not recorded. Choose a project.'
    : floor.onboarding === null
      ? 'Projects are not offered to this session. Check Setup before hiring.'
      : matches.length === 0
        ? 'The archived project label is no longer offered. Choose a project.'
        : matches.length > 1
          ? 'The archived label matches more than one current project. Choose the intended project.'
          : workspaceId === ''
            ? 'Choose a project, then review the engines it offers.'
            : 'This current project matches the archived label. The archive has no project ID; review this choice.';
  const engineNote = archived === null
    ? `The archived ${row.provider} engine cannot be hired from the browser. No engine was preselected.`
    : missingEngine
      ? `Dudes could not find ${HIRE_ENGINES[archived]} when this form opened. No app was preselected. Install it and check again.`
      : engine !== null
        ? `${HIRE_ENGINES[archived]}, the archived engine, is preselected. Review the engine choice.`
        : match === null
          ? `The archived engine was ${HIRE_ENGINES[archived]}. No engine was preselected; choose a project, then an engine it offers.`
          : `This project does not offer the archived ${HIRE_ENGINES[archived]} engine. No engine was preselected.`;
  commit({ ...INITIAL, draft: Object.freeze({ name: row.name, role: row.role, workspaceId,
    ...(engine === null ? {} : { engine }),
    ...(row.character === undefined ? {} : { character: row.character }),
    ...(row.accent === undefined ? {} : { accent: row.accent }) }),
    prefill: Object.freeze({ notes: Object.freeze({
      identity: 'Recorded appearance choices were copied from the archive. Unrecorded choices stay Automatic.',
      workspace, engine: engineNote,
    }) }) });
  return true;
}

export function prepareHire(floor: FloorState): void {
  if (state.attempt !== null || state.sending || state.draft.workspaceId !== '') return;
  const spaces = floor.onboarding?.workspaces ?? [];
  const space = spaces.length === 1 ? spaces[0]! : null;
  if (space === null) return;
  const engine = chosenEngine(state.draft, space, floor.daemon);
  commit({ ...state, draft: Object.freeze({ ...state.draft, workspaceId: space.workspaceId,
    ...(engine === null ? {} : { engine }) }) });
}

export function showSection(section: SectionKey, offer: HireOffer = getFloorState()): void {
  const at = SECTIONS.findIndex((entry) => entry.key === state.section);
  const next = SECTIONS.findIndex((entry) => entry.key === section);
  if (next > at) {
    const earlier = SECTIONS.slice(0, next).map((entry) => entry.key);
    const problem = problemsIn(state.draft, offer).find((entry) => earlier.includes(entry.section));
    if (problem !== undefined) {
      commit({ ...state, section: problem.section, problem });
      return;
    }
  }
  commit({ ...state, section, problem: null });
}

/** Back or forward one section; forward only past a section with no problem. */
export function moveSection(step: -1 | 1, offer: HireOffer): void {
  const at = SECTIONS.findIndex((entry) => entry.key === state.section);
  const next = SECTIONS[at + step];
  if (next !== undefined) showSection(next.key, offer);
}

/**
 * The one Hire. It refuses while an attempt is open, so a second press does
 * nothing; a draft with a problem moves to the section that owns the problem
 * and keeps every value typed.
 */
export async function submitHire(offer: HireOffer, deps: HireDeps): Promise<void> {
  if (state.sending || state.attempt !== null) return;
  const problem = problemsIn(state.draft, offer)[0];
  if (problem !== undefined) {
    commit({ ...state, section: problem.section, problem });
    return;
  }
  if (!deps.signedIn()) {
    commit({ ...state, problem: { field: null, section: state.section,
      message: 'This page has no Floor session, so nothing was sent.' } });
    return;
  }
  const workspace = chosenWorkspace(state.draft, offer.onboarding);
  const revision = workspace?.reloadMark?.revision;
  const engine = chosenEngine(state.draft, workspace, offer.daemon);
  const copy = ownCopyOffer(state.draft, workspace, engine, agentsIn(offer.agents, state.draft.workspaceId).length);
  const payload: HirePayload = Object.freeze({
    attempt_id: mintAttemptId(deps.random),
    workspace_id: state.draft.workspaceId,
    ...(revision === undefined ? {} : { workspace_revision: revision }),
    name: trimAsciiSpace(state.draft.name),
    role: trimAsciiSpace(state.draft.role) || DEFAULT_ROLE,
    ...(engine === 'codex' || engine === 'pi' ? { provider: engine } : {}),
    ...(copy.state === 'available' && copy.checked ? { own_copy: true as const } : {}),
    ...(trimAsciiSpace(state.draft.model ?? '') ? { model: trimAsciiSpace(state.draft.model!) } : {}),
    ...(state.draft.character ? { character: state.draft.character } : {}),
    ...(state.draft.accent ? { accent: state.draft.accent } : {}),
  });
  const attempt: HireAttempt = Object.freeze({ payload, body: JSON.stringify(payload) });
  commit({ ...state, attempt, sending: true, outcome: null, problem: null, revealed: null, uncertain: false });
  await send(attempt, deps);
}

/** Check status: the same attempt, the same body, sent again by the operator. */
export async function checkHireStatus(deps: HireDeps): Promise<void> {
  const attempt = state.attempt;
  if (attempt === null || state.sending) return;
  if (!deps.signedIn()) {
    commit({ ...state, outcome: { kind: 'unknown' } });
    return;
  }
  commit({ ...state, sending: true });
  await send(attempt, deps);
}

async function send(attempt: HireAttempt, deps: HireDeps): Promise<void> {
  const outcome = await postHire(attempt, deps.fetch);
  if (state.attempt !== attempt) return;
  // A durable hire changes the session's offer, a refusal may mean the
  // session has ended, and a stale workspace means the catalog on screen is
  // old: in each case the answer is in a fresh snapshot, and the refresh drops
  // the catalog until it arrives.
  const ask = outcome.kind === 'complete' || outcome.kind === 'refused'
    || outcome.kind === 'stale';
  const uncertain = state.uncertain
    || outcome.kind === 'pending' || outcome.kind === 'unavailable' || outcome.kind === 'unknown';
  const next: HireState = { ...state, sending: false, outcome, uncertain,
    askedAtEpoch: ask ? deps.epoch() : state.askedAtEpoch };
  commit(next);
  if (ask) deps.refresh();
}

/**
 * Whether the attempt may be let go: only when the daemon has confirmed that
 * it had no effect. An empty 409 always confirms that. A 403, 400 or 413 does
 * only when no earlier reply to the same attempt left its effect unknown: after
 * `pending`, a 503 or a lost response, it proves only that the later request
 * did nothing. The attempt is then kept, so that a fresh session sends it again
 * rather than a new Hire. Never while sending.
 */
export function releasable(hire: HireState): boolean {
  const kind = hire.outcome?.kind;
  if (hire.sending) return false;
  if (kind === 'conflict') return true;
  return (kind === 'refused' || kind === 'rejected' || kind === 'stale') && !hire.uncertain;
}

/**
 * Put the form back after an answer that proves nothing happened, keeping the
 * values typed. Any other answer keeps the attempt.
 */
export function releaseAttempt(): void {
  if (!releasable(state)) return;
  commit({ ...state, attempt: null, outcome: null, problem: null, revealed: null, uncertain: false });
}

/**
 * What closing the dialog does: a form nobody sent, and a hire that completed, are forgotten, so the
 * dialog reopens on the first step with empty fields. An attempt still waiting on an answer stays so it can
 * be checked and never repeated.
 */
export function leaveHire(): void {
  const kind = state.outcome?.kind;
  if (state.sending || (state.attempt !== null && kind !== 'complete') || state === INITIAL) return;
  commit({ ...INITIAL });
}

/** Start over after a hire the journal holds, with an empty form. */
export function startAnotherHire(): void {
  const kind = state.outcome?.kind;
  if (state.sending || kind !== 'complete') return;
  commit({ ...INITIAL });
}

/** Note that the agent from this outcome has been revealed on the floor. */
export function markRevealed(agent: string): void {
  const kind = state.outcome?.kind;
  if (kind !== 'complete') return;
  if (state.revealed !== agent) commit({ ...state, revealed: agent });
}

/** Send one attempt's body and read what the answer proves. */
export async function postHire(
  attempt: HireAttempt, fetch: HireDeps['fetch'],
): Promise<HireOutcome> {
  let status: number;
  let text: string;
  try {
    const response = await fetch('/floor/onboard', {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json' },
      body: attempt.body,
    });
    status = response.status;
    text = await response.text();
  } catch {
    return Object.freeze({ kind: 'unknown' });
  }
  return decodeHireResponse(status, text, attempt.payload.attempt_id);
}

function exactly(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const own = Object.keys(value);
  return own.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function isId(value: unknown): value is string {
  return typeof value === 'string' && AGENT_ID.test(value);
}

function json(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

/** The response map. Anything that does not match its row exactly proves nothing. */
export function decodeHireResponse(status: number, text: string, attemptId: string): HireOutcome {
  const unknown: HireOutcome = Object.freeze({ kind: 'unknown' });
  switch (status) {
    case 200: {
      const body = json(text);
      return exactly(body, ['attempt_id', 'state', 'agent']) && body.attempt_id === attemptId
        && body.state === 'complete' && isId(body.agent)
        ? Object.freeze({ kind: 'complete', agent: body.agent }) : unknown;
    }
    case 202: {
      const body = json(text);
      return exactly(body, ['attempt_id', 'state']) && body.attempt_id === attemptId && body.state === 'pending'
        ? Object.freeze({ kind: 'pending' }) : unknown;
    }
    case 403:
      return Object.freeze({ kind: 'refused' });
    case 409: {
      if (text === '') return Object.freeze({ kind: 'conflict' });
      const body = json(text);
      if (exactly(body, ['attempt_id', 'state', 'reason', 'note'])
        && body.attempt_id === attemptId && body.state === 'refused'
        && (body.reason === 'shared_folder' || body.reason === 'own_copy' || body.reason === 'model_fixed')
        && typeof body.note === 'string' && body.note.length > 0 && body.note.length <= 512
        && !/[\u0000-\u001f\u007f]/u.test(body.note)) {
        return Object.freeze({ kind: 'refused', note: body.note });
      }
      if (exactly(body, ['attempt_id', 'state', 'reason']) && body.attempt_id === attemptId
        && body.state === 'refused') {
        if (body.reason === 'profile_changed') return Object.freeze({ kind: 'stale' });
        if (body.reason === 'action_off') return Object.freeze({ kind: 'refused',
          note: 'Hiring was turned off. Turn on Hire agents under Floor switches in Settings, then try again.' });
      }
      return unknown;
    }
    case 503:
      return Object.freeze({ kind: 'unavailable' });
    case 400:
    case 413:
      return Object.freeze({ kind: 'rejected', status });
    default:
      return unknown;
  }
}

/** Whether a snapshot taken after `sinceEpoch` is the view on screen now. */
export function freshSince(floor: FloorState, sinceEpoch: number): boolean {
  return floor.phase === 'live' && floor.snapshotEpoch === floor.epoch && floor.snapshotEpoch > sinceEpoch;
}

/**
 * After a refusal, what the fresh subscription says about the session: a
 * snapshot means it is alive and the daemon refused the Hire itself; a stream
 * that gave up means the session has ended.
 */
export function sessionAfterRefusal(floor: FloorState, sinceEpoch: number): 'checking' | 'live' | 'ended' {
  if (freshSince(floor, sinceEpoch)) return 'live';
  if (floor.phase === 'closed') return 'ended';
  return 'checking';
}

/**
 * Where a hired agent is now, by the projection. `absent` only once a snapshot
 * taken after the daemon answered has left it out: it has been retired since.
 */
export function presenceOf(agent: string, floor: FloorState, sinceEpoch: number): 'present' | 'absent' | 'unconfirmed' {
  if (floor.agents.some((row) => row.id === agent)) return 'present';
  return freshSince(floor, sinceEpoch) ? 'absent' : 'unconfirmed';
}

/** The agent this outcome should select on the floor now, if any. */
export function agentToReveal(hire: HireState, floor: FloorState): string | null {
  const outcome = hire.outcome;
  if (outcome === null || outcome.kind !== 'complete') return null;
  if ((hire.revealed === outcome.agent)
    || floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return null;
  return floor.agents.some((row) => row.id === outcome.agent) ? outcome.agent : null;
}
