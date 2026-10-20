// SPDX-License-Identifier: MIT OR Apache-2.0
//
// One Hire: four sections of choices, one request, one truthful answer.
//
// The page names an agent, gives it a role, picks one workspace the operator
// enrolled, one engine the daemon offers for it and one completion check the
// operator enrolled for it, and writes its first goal. Everything else that
// decides how the agent runs — folder, command, the check's command, ceilings,
// isolation, trust — is the daemon's configuration and never a field here. The
// daemon runs the chosen check once before it hires, writes the agent and its
// goal to its own journal; the page only asks, and then says what the answer
// proves.
//
// What the answer proves is narrow on purpose. Only `complete` is a success.
// `pending`, a 503 and a lost response prove nothing either way, so the page
// keeps the attempt and offers to ask again with it; a durable agent without
// its goal is shown as exactly that. A refusal the daemon made before any
// effect lets the operator edit and try again.
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
import { checkRefusalText, checksFor, isCheckRefusal } from './checks.ts';
import type { CheckCatalog, CheckRefusal, EnrolledCheck } from './checks.ts';
import { floorFetch } from '../../transport.ts';

export type SectionKey = 'identity' | 'workspace' | 'engine' | 'briefing';

/** The four sections, in order; one is shown at a time. */
export const SECTIONS: readonly { readonly key: SectionKey; readonly label: string; readonly hint: string }[] =
  Object.freeze([
    { key: 'identity', label: 'Who', hint: 'Name and look' },
    { key: 'workspace', label: 'Project', hint: 'Where they work' },
    { key: 'engine', label: 'App', hint: 'Claude, Codex or Pi' },
    { key: 'briefing', label: 'First task', hint: 'What to do first' },
  ]);

/** What an agent is called when the person gives no role. */
export const DEFAULT_ROLE = 'engineer';

/** Field limits in UTF-8 bytes, measured after trimming ASCII whitespace. */
export const LIMITS = Object.freeze({ name: 128, role: 512, goal_text: 2048 });

/** What to tell the person about a field's length: nothing until it is near the limit. */
export function lengthNote(value: string, limit: number): string | null {
  const used = utf8Bytes(trimAsciiSpace(value));
  if (used > limit) return 'This is too long. Shorten it.';
  return used >= limit * 0.9 ? 'This is almost too long.' : null;
}

export type FieldName = 'name' | 'role' | 'workspace_id' | 'engine' | 'check_id' | 'goal_text';

/** What the page offers a Hire from: the enrolled workspaces, and the checks each offers. */
export type HireOffer = Pick<FloorState, 'onboarding' | 'checks' | 'agents'>;

export const NO_CHECK = 'No completion check is offered for this project, so Hire is unavailable.';
export const NO_PROJECT_LIST = 'Hire cannot list your projects right now. Reload the page, and check Settings if this stays.';

export interface HireDraft {
  readonly name: string;
  readonly role: string;
  readonly workspaceId: string;
  /** The engine chosen, which counts only while the chosen workspace offers it. */
  readonly engine?: HireEngine;
  /** The check chosen, which counts only while the chosen workspace offers it. */
  readonly checkId?: string;
  readonly goalText: string;
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
  /** The check refusal this problem reports, when it reports one. */
  readonly refusal?: CheckRefusal;
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
  readonly check_id: string;
  readonly check_revision: string;
  readonly name: string;
  readonly role: string;
  readonly goal_text: string;
  readonly provider?: Exclude<HireEngine, 'claude'>;
  /** Sent only when the agent should work in its own copy of the project. */
  readonly own_copy?: true;
  readonly character?: string;
  readonly accent?: AccentColour;
}

/** One attempt: its frozen payload, the serialized body every send reuses, and its check's label. */
export interface HireAttempt {
  readonly payload: HirePayload;
  readonly body: string;
  readonly checkLabel: string;
}

export type PartialReason =
  | 'agent_retired' | 'goal_conflict' | 'session_revoked'
  | 'session_expired' | 'profile_changed' | 'goal_refused';

export const PARTIAL_REASONS: Readonly<Record<PartialReason, string>> = Object.freeze({
  agent_retired: 'The agent was retired before its goal was set.',
  goal_conflict: 'Another goal was set on this agent first, and the Hire did not overwrite it.',
  session_revoked: 'This Floor session was revoked after the agent was hired and before its goal was set.',
  session_expired: 'This Floor session expired after the agent was hired and before its goal was set.',
  profile_changed: 'The project changed after the agent was hired, so its goal was not set.',
  goal_refused: 'The daemon refused the initial goal, for example on a budget or reserve check.',
});

/** What one answer proves. Only `complete` is a success. */
export type HireOutcome =
  | { readonly kind: 'complete'; readonly agent: string; readonly goal: string }
  | {
    readonly kind: 'partial'; readonly agent: string; readonly reason: PartialReason;
    readonly currentGoal: string | null; readonly retired: boolean;
  }
  /** 202: in flight or unresolved. No agent. */
  | { readonly kind: 'pending' }
  /** 503: durability could not be established. No agent. */
  | { readonly kind: 'unavailable' }
  /** A lost response, an unexpected status or an unreadable body. No agent. */
  | { readonly kind: 'unknown' }
  /** Empty 403 or a structured 409 shared-folder or own-copy refusal, before any effect. */
  | { readonly kind: 'refused'; readonly note?: string }
  /** Empty 409: this attempt names a different request. Refused before any effect. */
  | { readonly kind: 'conflict' }
  /**
   * 409 refused `profile_changed`: a reload changed the workspace after this
   * page was offered it. Refused before any effect.
   */
  | { readonly kind: 'stale' }
  /**
   * 409 refused by the chosen check's run, or by this session's limits on
   * running checks. Refused before any effect.
   */
  | { readonly kind: 'check'; readonly reason: CheckRefusal }
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
  readonly revealedKind: 'partial' | 'complete' | null;
  readonly prefill: { readonly notes: Readonly<Partial<Record<SectionKey, string>>> } | null;
  /**
   * Whether any reply to this attempt left its effect unknown: `pending`, a 503
   * or a lost response. Once it has, a later refusal proves only that the later
   * request did nothing — the earlier one may already have hired.
   */
  readonly uncertain: boolean;
  /**
   * The AgentId the daemon recorded for this attempt — from `hired_without_goal`
   * or `complete` — kept whatever later replies say. An attempt that has hired
   * an agent is never let go: only its own Check may finish it.
   */
  readonly recordedAgent: string | null;
}

export interface HireResponse {
  readonly status: number;
  text(): Promise<string>;
}

export interface HireDeps {
  fetch(url: string, init: RequestInit): Promise<HireResponse>;
  /** The session's CSRF nonce, or `null` when the page has no session. */
  csrf(): string | null;
  /** Fill `bytes` with cryptographically random values. */
  random(bytes: Uint8Array<ArrayBuffer>): void;
  /** Ask the stream for a fresh snapshot of this session. */
  refresh(): void;
  /** The floor's current subscription epoch. */
  epoch(): number;
}

export const browserHireDeps: HireDeps = {
  fetch: floorFetch,
  csrf: () => currentFloorSession()?.csrf ?? null,
  random: (bytes) => {
    crypto.getRandomValues(bytes);
  },
  refresh: () => {
    refreshFloorSnapshot();
  },
  epoch: () => getFloorState().epoch,
};

const EMPTY_DRAFT: HireDraft = Object.freeze({ name: '', role: '', workspaceId: '', goalText: '' });
const INITIAL: HireState = Object.freeze({
  draft: EMPTY_DRAFT, section: 'identity', problem: null, attempt: null,
  sending: false, outcome: null, askedAtEpoch: 0, revealed: null, uncertain: false, recordedAgent: null,
  revealedKind: null, prefill: null,
});

/** The canonical attempt form: 32 bytes, unpadded base64url, no stray bits. */
export const ATTEMPT_ID = /^[A-Za-z0-9_-]{42}[AEIMQUYcgkosw048]$/;
const PARTIAL_KEYS = Object.freeze(['attempt_id', 'state', 'agent', 'goal', 'reason', 'current_goal', 'retired']);
const REFUSED_KEYS = Object.freeze(['attempt_id', 'state', 'reason']);
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

/** The check this draft would run first: the one chosen, while its workspace offers it, or else the first. */
export function chosenCheck(draft: HireDraft, checks: CheckCatalog | null): EnrolledCheck | null {
  const offered = checksFor(checks, draft.workspaceId) ?? [];
  return offered.find((check) => check.id === draft.checkId) ?? offered[0] ?? null;
}

/**
 * The engine this draft would hire: the one chosen, when the workspace offers
 * it, or else the workspace's only engine. `null` while there is no choice.
 */
export function chosenEngine(draft: HireDraft, workspace: EnrolledWorkspace | null): HireEngine | null {
  if (workspace === null) return null;
  if (draft.engine !== undefined && workspace.engines.includes(draft.engine)) return draft.engine;
  return workspace.engines.length === 1 ? workspace.engines[0]! : null;
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
 * - `available`: the box shows. It starts ticked, with its reason, when another agent already
 *   works in the project's folder and the person has not decided; their own choice wins after that.
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
  return { state: 'available', checked: draft.ownCopy ?? preselected, preselected };
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
  } else if (chosenEngine(draft, workspace) === null) {
    problems.push({ field: 'engine', section: 'engine', message: 'Choose an engine this project offers.' });
  }
  if (workspace !== null && chosenCheck(draft, offer.checks) === null) {
    problems.push({ field: 'check_id', section: 'briefing', message: NO_CHECK });
  }
  const goal = bounded('The first task', draft.goalText, LIMITS.goal_text, 'Say what the agent should do first.');
  if (goal !== null) problems.push({ field: 'goal_text', section: 'briefing', message: goal });
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
  // A choice about one project's copy says nothing about another's.
  if (patch.workspaceId !== undefined && patch.workspaceId !== state.draft.workspaceId && !('ownCopy' in patch)) {
    delete next.ownCopy;
  }
  commit({ ...state, draft: Object.freeze(next), problem: null });
}

export function prepareArchiveHire(row: RetiredAgentRow, floor: FloorState): boolean {
  if (floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return false;
  const settled = state.outcome?.kind === 'complete' || state.outcome?.kind === 'partial';
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
      ? `${HIRE_ENGINES[archived]} was not found on the daemon’s PATH when this form was prepared. No engine was preselected; install it and check again.`
      : engine !== null
        ? `${HIRE_ENGINES[archived]}, the archived engine, is preselected. Review the engine and the current limits.`
        : match === null
          ? `The archived engine was ${HIRE_ENGINES[archived]}. No engine was preselected; choose a project, then an engine it offers.`
          : `This project does not offer the archived ${HIRE_ENGINES[archived]} engine. No engine was preselected.`;
  commit({ ...INITIAL, draft: Object.freeze({ name: row.name, role: row.role, workspaceId,
    ...(engine === null ? {} : { engine }), goalText: '',
    ...(row.character === undefined ? {} : { character: row.character }),
    ...(row.accent === undefined ? {} : { accent: row.accent }) }),
    prefill: Object.freeze({ notes: Object.freeze({
      identity: 'Recorded appearance choices were copied from the archive. Unrecorded choices stay Automatic.',
      workspace, engine: engineNote,
      briefing: 'The archive does not record the previous goal. Enter a new initial goal.',
    }) }) });
  return true;
}

/**
 * Choices a person should not have to make: the project when the offer has only one, and the first
 * engine the daemon found. Applied once, when the dialog opens on a draft with nothing chosen.
 */
export function prepareHire(floor: FloorState): void {
  if (state.attempt !== null || state.sending || state.draft.workspaceId !== '') return;
  const spaces = floor.onboarding?.workspaces ?? [];
  const space = spaces.length === 1 ? spaces[0]! : null;
  if (space === null) return;
  const engine = space.engines.find((name) => rowFor(floor.daemon, name)?.found === true);
  commit({ ...state, draft: Object.freeze({ ...state.draft, workspaceId: space.workspaceId,
    ...(engine === undefined ? {} : { engine }) }) });
}

export function showSection(section: SectionKey): void {
  commit({ ...state, section });
}

/** Back or forward one section; forward only past a section with no problem. */
export function moveSection(step: -1 | 1, offer: HireOffer): void {
  const at = SECTIONS.findIndex((entry) => entry.key === state.section);
  const next = SECTIONS[at + step];
  if (next === undefined) return;
  if (step === 1) {
    const problem = problemsIn(state.draft, offer).find((entry) => entry.section === state.section);
    if (problem !== undefined) {
      commit({ ...state, problem });
      return;
    }
  }
  commit({ ...state, section: next.key, problem: null });
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
  const csrf = deps.csrf();
  if (csrf === null) {
    commit({ ...state, problem: { field: null, section: state.section,
      message: 'This page has no Floor session, so nothing was sent.' } });
    return;
  }
  const workspace = chosenWorkspace(state.draft, offer.onboarding);
  const revision = workspace?.reloadMark?.revision;
  const engine = chosenEngine(state.draft, workspace);
  const check = chosenCheck(state.draft, offer.checks)!;
  const copy = ownCopyOffer(state.draft, workspace, engine, agentsIn(offer.agents, state.draft.workspaceId).length);
  const payload: HirePayload = Object.freeze({
    attempt_id: mintAttemptId(deps.random),
    workspace_id: state.draft.workspaceId,
    ...(revision === undefined ? {} : { workspace_revision: revision }),
    check_id: check.id,
    check_revision: check.revision,
    name: trimAsciiSpace(state.draft.name),
    role: trimAsciiSpace(state.draft.role) || DEFAULT_ROLE,
    goal_text: trimAsciiSpace(state.draft.goalText),
    ...(engine === 'codex' || engine === 'pi' ? { provider: engine } : {}),
    ...(copy.state === 'available' && copy.checked ? { own_copy: true as const } : {}),
    ...(state.draft.character ? { character: state.draft.character } : {}),
    ...(state.draft.accent ? { accent: state.draft.accent } : {}),
  });
  const attempt: HireAttempt = Object.freeze({ payload, body: JSON.stringify(payload), checkLabel: check.label });
  commit({ ...state, attempt, sending: true, outcome: null, problem: null, revealed: null, revealedKind: null, uncertain: false,
    recordedAgent: null });
  await send(attempt, csrf, deps);
}

/** Check status: the same attempt, the same body, sent again by the operator. */
export async function checkHireStatus(deps: HireDeps): Promise<void> {
  const attempt = state.attempt;
  if (attempt === null || state.sending) return;
  const csrf = deps.csrf();
  if (csrf === null) {
    commit({ ...state, outcome: { kind: 'unknown' } });
    return;
  }
  commit({ ...state, sending: true });
  await send(attempt, csrf, deps);
}

async function send(attempt: HireAttempt, csrf: string, deps: HireDeps): Promise<void> {
  const outcome = await postHire(attempt, csrf, deps.fetch);
  if (state.attempt !== attempt) return;
  // A durable hire changes the session's offer, a refusal may mean the
  // session has ended, and a stale workspace means the catalog on screen is
  // old: in each case the answer is in a fresh snapshot, and the refresh drops
  // the catalog until it arrives.
  const ask = outcome.kind === 'complete' || outcome.kind === 'partial' || outcome.kind === 'refused'
    || outcome.kind === 'stale';
  const uncertain = state.uncertain
    || outcome.kind === 'pending' || outcome.kind === 'unavailable' || outcome.kind === 'unknown';
  const recordedAgent = outcome.kind === 'complete' || outcome.kind === 'partial' ? outcome.agent : state.recordedAgent;
  const next: HireState = { ...state, sending: false, outcome, uncertain, recordedAgent,
    askedAtEpoch: ask ? deps.epoch() : state.askedAtEpoch };
  if (outcome.kind === 'check' && releasable(next)) {
    // The offer on screen is still current, unlike after a stale workspace, so
    // the form comes back at its check choice without waiting for a snapshot.
    const sessionLimit = outcome.reason === 'check_running' || outcome.reason === 'check_limit';
    commit({ ...next, attempt: null, outcome: null, revealed: null, revealedKind: null, section: 'briefing',
      problem: Object.freeze({ field: sessionLimit ? null : 'check_id', section: 'briefing', refusal: outcome.reason,
        message: checkRefusalText(outcome.reason, attempt.checkLabel, 'hire') }) });
    return;
  }
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
  // An attempt the journal holds an agent for is never let go, whatever came after.
  if (hire.sending || hire.recordedAgent !== null) return false;
  if (kind === 'conflict') return true;
  return (kind === 'refused' || kind === 'rejected' || kind === 'stale' || kind === 'check') && !hire.uncertain;
}

/**
 * Put the form back after an answer that proves nothing happened, keeping the
 * values typed. Any other answer keeps the attempt.
 */
export function releaseAttempt(): void {
  if (!releasable(state)) return;
  commit({ ...state, attempt: null, outcome: null, problem: null, revealed: null, revealedKind: null, uncertain: false,
    recordedAgent: null });
}

/** Start over after a hire the journal holds, with an empty form. */
export function startAnotherHire(): void {
  const kind = state.outcome?.kind;
  if (state.sending || (kind !== 'complete' && kind !== 'partial')) return;
  commit({ ...INITIAL });
}

/** Note that the agent from this outcome has been revealed on the floor. */
export function markRevealed(agent: string): void {
  const kind = state.outcome?.kind;
  if (kind !== 'complete' && kind !== 'partial') return;
  if (state.revealed !== agent || state.revealedKind !== kind) commit({ ...state, revealed: agent, revealedKind: kind });
}

/** Send one attempt's body and read what the answer proves. */
export async function postHire(
  attempt: HireAttempt, csrf: string, fetch: HireDeps['fetch'],
): Promise<HireOutcome> {
  let status: number;
  let text: string;
  try {
    const response = await fetch('/floor/onboard', {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      referrerPolicy: 'no-referrer',
      headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf },
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
      return exactly(body, ['attempt_id', 'state', 'agent', 'goal']) && body.attempt_id === attemptId
        && body.state === 'complete' && isId(body.agent) && isId(body.goal)
        ? Object.freeze({ kind: 'complete', agent: body.agent, goal: body.goal }) : unknown;
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
        && (body.reason === 'shared_folder' || body.reason === 'own_copy')
        && typeof body.note === 'string' && body.note.length > 0 && body.note.length <= 512
        && !/[\u0000-\u001f\u007f]/u.test(body.note)) {
        return Object.freeze({ kind: 'refused', note: body.note });
      }
      if (exactly(body, REFUSED_KEYS)) {
        if (body.attempt_id !== attemptId || body.state !== 'refused') return unknown;
        if (body.reason === 'profile_changed') return Object.freeze({ kind: 'stale' });
        return isCheckRefusal(body.reason) ? Object.freeze({ kind: 'check', reason: body.reason }) : unknown;
      }
      if (!exactly(body, PARTIAL_KEYS) || body.attempt_id !== attemptId || body.state !== 'hired_without_goal'
        || !isId(body.agent) || body.goal !== null || typeof body.retired !== 'boolean'
        || typeof body.reason !== 'string' || !Object.hasOwn(PARTIAL_REASONS, body.reason)
        || (body.current_goal !== null && !isId(body.current_goal))) {
        return unknown;
      }
      return Object.freeze({ kind: 'partial', agent: body.agent, reason: body.reason as PartialReason,
        currentGoal: body.current_goal as string | null, retired: body.retired });
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

/**
 * Whether an attempt that hired an agent may Check now: only on a snapshot
 * taken after the daemon last answered, from a live session that offers
 * hiring. A session that has ended would only refuse it again.
 */
export function mayCheckRecorded(hire: HireState, floor: FloorState): boolean {
  return freshSince(floor, hire.askedAtEpoch) && floor.onboarding !== null;
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
  if (outcome === null || (outcome.kind !== 'complete' && outcome.kind !== 'partial')) return null;
  if ((hire.revealed === outcome.agent && hire.revealedKind === outcome.kind)
    || floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return null;
  return floor.agents.some((row) => row.id === outcome.agent) ? outcome.agent : null;
}
