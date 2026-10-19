// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Change one agent's goal text, or renew a goal that has nothing left: when the
// page may offer each, what carries over or what the workspace grants, one
// request, and what its outcome proves.
//
// Goal change is the operator's opt-in. A snapshot carries the goal limits only
// when the daemon routes the action, and without them there is no form at all.
// A request names its agent and the agent's current goal exactly as the
// projection sent it; the daemon applies it only while that goal is still the
// agent's current one and no turn is open. The new goal inherits only what the
// replaced goal had left, so the page states that before anything is sent. It
// keeps the replaced goal's completion check, or runs one the operator enrolled
// for the agent's workspace; either way the daemon runs that check once first
// and refuses a check that already passes.
//
// A goal with nothing left cannot carry anything over. When the operator also
// turned renewal on, the page offers a second request in the change's place: a
// new goal with the allowance the agent's enrolled workspace gives, which the
// page restates in the body exactly as the snapshot showed it. Both requests
// share one discipline and one outcome map; only the route, the body and the
// reasons differ.
//
// One request is in flight or unresolved at a time on this page. It and the
// unsent text live here, in module memory, and nowhere else: not in the URL,
// not in browser storage, not in a log. The body is frozen when Send mints it,
// and Check status sends those same bytes again. A reload loses both; the page
// then offers only what the projection says, and never sends anything by
// itself.
//
// What an outcome proves is narrow on purpose. `changed` means the daemon holds
// the new goal durably, and it does not mean that a turn started. Whether the
// goal moved, and what holds the agent now, is read from the projection alone.

import { currentFloorSession, floorSessionGeneration } from '../../session.ts';
import type { RenewAllowance } from './limits.ts';
import type { FloorState } from '../floor/store.ts';
import { AGENT_ID, getFloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { checkRefusalText, checksFor, isCheckRefusal } from '../onboarding/checks.ts';
import type { CheckRefusal, EnrolledCheck } from '../onboarding/checks.ts';
import { mintAttemptId, trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
import type { AgentRow } from '../scene/cast.ts';
import { decodeWorkspace } from '../workspace.ts';
import { completionCheckOf } from './completionCheck.ts';
import { floorFetch } from '../../transport.ts';

/** A `GoalId` has the same canonical text form as an `AgentId`. */
const GOAL_ID = AGENT_ID;

/** The line fixed for a change that must wait for the running turn, drawn and refused alike. */
export const TURN_RUNNING = 'You can change the goal when the current turn ends';

/** What the new goal inherits from the one it replaces, read from the projection's goal view. */
export interface CarryOver {
  /** The hop cap less the turns issued on the goal: exact. */
  readonly turns: number;
  /**
   * The dollar cap less the dollars spent, in micro-dollars, or `null` when the
   * goal caps no dollars. An upper bound: the daemon also holds back a reserve
   * for turn ends it has not observed, which the projection does not carry.
   */
  readonly usdAtMost: number | null;
}

/**
 * What the current goal has run out of: its turns, its capped dollars, both,
 * or a cap the goal view does not show, known only from the daemon's
 * `goal_exhausted` answer.
 */
export type Spent = 'turns' | 'dollars' | 'both' | 'cap';

/** What a new goal would get, restated from the snapshot's `renew` entry for the agent's workspace. */
export interface RenewOffer extends RenewAllowance {
  /** The current goal's text, which starts the field. */
  readonly text: string;
  readonly spent: Spent;
}

export type GoalGate =
  | { readonly open: false }
  | {
    readonly open: true;
    readonly agent: string;
    /** The agent's current goal, exactly as the projection sent it. */
    readonly goal: string;
    readonly maxTextBytes: number;
    readonly carry: CarryOver;
    /** A turn is running, so the form is drawn but cannot send. */
    readonly turnRunning: boolean;
    /** The agent's turn count when the form was drawn, for telling a new turn apart. */
    readonly turns: number;
    /** Present when the form renews the goal instead of changing its text. */
    readonly renew?: RenewOffer;
    /** The label of the goal's current check, or `null` when it was recorded without one. */
    readonly currentCheck: string | null;
    /** The checks of the agent's workspace, or none when the offer has no entry for it. */
    readonly enrolled: readonly EnrolledCheck[];
    /** The goal is achieved, so its current check passed: nothing is chosen until the operator chooses. */
    readonly achieved: boolean;
  };

export type OpenGoalGate = Extract<GoalGate, { readonly open: true }>;

/** Keep the goal's current check, or run an enrolled one, named by its id. */
export type CheckChoice = 'keep' | { readonly id: string };

const CLOSED: GoalGate = Object.freeze({ open: false });

function count(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value) && value >= 0;
}

function record(value: unknown): Record<string, unknown> | null {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown> : null;
}

/** The goal a row pursues, or `null` when it has none or its identity is not canonical. */
export function goalIdOf(row: AgentRow): string | null {
  const id = record(row.goal)?.id;
  return typeof id === 'string' && GOAL_ID.test(id) ? id : null;
}

/**
 * What carries over from one goal view, or `null` when a figure it needs is
 * unreadable. An absent or null dollar cap is no cap; a zero cap is a cap of
 * nothing, and the two are never read as each other.
 */
export function carryOverOf(goal: Record<string, unknown>): CarryOver | null {
  const hopCap = goal.hop_cap;
  const turns = goal.turns === undefined ? 0 : goal.turns;
  const budget = record(goal.budget);
  const spent = goal.spent === undefined ? {} : record(goal.spent);
  if (!count(hopCap) || !count(turns) || budget === null || spent === null) return null;
  const left = Math.max(0, hopCap - turns);
  const cap = budget.usd ?? null;
  if (cap === null) return Object.freeze({ turns: left, usdAtMost: null });
  const used = spent.usd === undefined ? 0 : spent.usd;
  if (!count(cap) || !count(used)) return null;
  return Object.freeze({ turns: left, usdAtMost: Math.max(0, cap - used) });
}

/**
 * Whether this agent's detail may show the goal form, and for which goal.
 *
 * Open only when every one of these holds: the snapshot this view came from
 * carries the goal limits; the view is live and confirmed; the agent is on the
 * floor, so not retired; no halt stands on it; and it pursues a goal whose
 * identity and carry-over figures are readable. A running turn leaves the gate
 * open with `turnRunning`, so the form says why it waits.
 *
 * The gate carries `renew` instead of a change exactly when the snapshot lists
 * the agent's recorded workspace for renewal and the goal has nothing left:
 * no turns or a capped dollar figure of zero in the goal view, or an earlier
 * `goal_exhausted` answer to a change of this very goal, which is how a spent
 * token cap shows, because the goal view carries no token figure.
 */
export function goalGate(floor: FloorState, row: AgentRow | null, memory: GoalState = state): GoalGate {
  const limits = floor.goalChange;
  if (!limits) return CLOSED;
  if (floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return CLOSED;
  if (row === null || !floor.agents.some((agent) => agent.id === row.id)) return CLOSED;
  if (row.halted === true) return CLOSED;
  const goal = goalIdOf(row);
  const view = record(row.goal);
  const carry = view === null ? null : carryOverOf(view);
  if (goal === null || view === null || carry === null) return CLOSED;
  const renew = renewOffer(limits.renew, row, view, carry, memory.exhausted.has(draftKey(row.id, goal)));
  const workspace = decodeWorkspace(row.workspace);
  return Object.freeze({
    open: true, agent: row.id, goal, maxTextBytes: limits.maxTextBytes, carry,
    turnRunning: row.status === 'running', turns: typeof row.turns === 'number' ? row.turns : 0,
    currentCheck: completionCheckOf(view)?.label ?? null,
    enrolled: (workspace === null ? null : checksFor(floor.checks, workspace.id)) ?? [],
    achieved: view.status === 'achieved',
    ...(renew === null ? {} : { renew }),
  });
}

function renewOffer(
  workspaces: readonly RenewAllowance[] | undefined, row: AgentRow, view: Record<string, unknown> | null,
  carry: CarryOver, refused: boolean,
): RenewOffer | null {
  if (workspaces === undefined || view === null) return null;
  const spent = spentOf(carry) ?? (refused ? 'cap' : null);
  if (spent === null) return null;
  const id = decodeWorkspace(row.workspace)?.id;
  const entry = workspaces.find((candidate) => candidate.workspaceId === id);
  if (entry === undefined) return null;
  return Object.freeze({ ...entry, text: typeof view.text === 'string' ? view.text : '', spent });
}

function spentOf(carry: CarryOver): Spent | null {
  const turns = carry.turns === 0;
  const dollars = carry.usdAtMost === 0;
  if (turns && dollars) return 'both';
  if (turns) return 'turns';
  return dollars ? 'dollars' : null;
}

/** The change's body, exactly: four fields, and an enrolled check's id and revision when one is chosen. */
export interface ChangePayload {
  readonly request_id: string;
  readonly agent: string;
  readonly goal: string;
  readonly text: string;
  readonly check_id?: string;
  readonly check_revision?: string;
}

/**
 * The renewal's body, exactly: the change's four fields and the three numbers
 * the page displayed, copied verbatim from the snapshot, in this order.
 */
export interface RenewPayload extends ChangePayload {
  readonly hop_cap: string;
  readonly goal_usd_micros: string | null;
  readonly goal_tokens: string | null;
}

export type GoalPayload = ChangePayload | RenewPayload;

const CHANGE_ROUTE = '/floor/goal';
const RENEW_ROUTE = '/floor/renew';

export function isRenewal(payload: GoalPayload): payload is RenewPayload {
  return 'hop_cap' in payload;
}

function routeOf(payload: GoalPayload): string {
  return isRenewal(payload) ? RENEW_ROUTE : CHANGE_ROUTE;
}

/** One request: its frozen payload, the body every send reuses, and what it was sent with. */
export interface GoalAttempt {
  readonly payload: GoalPayload;
  readonly body: string;
  readonly turnsAtSend: number;
  /** The label of the check this request runs, or `null` for a kept check recorded without one. */
  readonly checkLabel: string | null;
  /** Whether the agent's workspace had checks to choose from when this request was sent. */
  readonly enrolled: boolean;
}

type FixedRefusal = 'agent_absent' | 'agent_halted' | 'stale_goal' | 'turn_open' | 'predicate_claimed'
  | 'predicate_todo' | 'goal_exhausted' | 'goal_not_exhausted' | 'workspace_unavailable' | 'profile_changed';

/** The reasons each route can answer with, besides a check's. A reason from the other route proves nothing. */
const CHANGE_REASONS: ReadonlySet<string> = new Set(['agent_absent', 'agent_halted', 'stale_goal', 'turn_open',
  'predicate_claimed', 'predicate_todo', 'workspace_unavailable', 'profile_changed', 'goal_exhausted']);
const RENEW_REASONS: ReadonlySet<string> = new Set(['agent_absent', 'agent_halted', 'stale_goal', 'turn_open',
  'predicate_claimed', 'predicate_todo', 'goal_not_exhausted', 'workspace_unavailable', 'profile_changed']);

export type GoalRefusalReason = FixedRefusal | CheckRefusal;

export const REFUSAL_REASONS: Readonly<Record<FixedRefusal, string>> = Object.freeze({
  stale_goal: 'This agent’s goal changed since this page read it, so nothing was recorded. '
    + 'The floor is being refreshed.',
  turn_open: TURN_RUNNING,
  predicate_claimed: 'This goal uses a Claimed card status, which cannot prove completion, so nothing was recorded.',
  predicate_todo: 'This goal uses a Todo card status, which cannot prove completion, so nothing was recorded.',
  goal_exhausted: 'This goal has no turns or capped dollars left to carry over, so nothing was recorded. '
    + 'To continue, give the agent a new goal with its own limits from a terminal (dudes goal), '
    + 'or retire it and hire a new one.',
  agent_halted: 'A halt stands on this agent, which the Floor cannot lift, so nothing was recorded.',
  agent_absent: 'The agent is no longer on the floor, so nothing was recorded.',
  goal_not_exhausted: 'This goal still has turns or dollars left, so nothing was recorded. Change its text instead.',
  workspace_unavailable: 'This agent’s project is no longer enrolled at its directory, so that check cannot be '
    + 'chosen. Nothing changed.',
  profile_changed: 'The operator changed that check after this page read it. Nothing changed. Choose again.',
});

/** What a renewal's own reasons say, where they differ from a change's. */
const RENEWAL_REASONS: Readonly<Partial<Record<FixedRefusal, string>>> = Object.freeze({
  workspace_unavailable: 'This agent’s project is not enrolled at its directory now, or sets no ceiling that the '
    + 'daemon’s billing key requires, so nothing was recorded. Give it a new goal from a terminal (dudes goal), '
    + 'or retire it and hire a new one.',
  profile_changed: 'The project’s allowance or checks changed since this page read it, so nothing was '
    + 'recorded. The floor is being refreshed.',
});

/** What `goal_exhausted` says when the page can offer a new goal beside it. */
export const EXHAUSTED_WITH_RENEWAL = 'This goal has no turns or capped dollars left to carry over, so nothing was '
  + 'recorded. Start a new goal with this project’s allowance below.';

const NOT_ENROLLED = 'This agent’s check already passes, and no enrolled project offers this agent another check. '
  + 'Nothing changed. Set a new goal from a terminal (dudes goal) with a check that fails until the goal is met.';

/** What one refusal of this request says. */
export function refusalText(reason: GoalRefusalReason, attempt: GoalAttempt): string {
  if (!isCheckRefusal(reason)) {
    return (isRenewal(attempt.payload) ? RENEWAL_REASONS[reason] : undefined) ?? REFUSAL_REASONS[reason];
  }
  if (reason === 'check_already_passes' && !attempt.enrolled) return NOT_ENROLLED;
  return checkRefusalText(reason, attempt.checkLabel, 'goal');
}

/**
 * What one response proves. Only `changed` means the daemon holds the new
 * goal: the change's `changed` state and the renewal's `renewed` both decode
 * to it, and the attempt says which request it answered.
 */
export type GoalOutcome =
  /** 200: this request's new goal is durable. Not a started turn. */
  | { readonly kind: 'changed' }
  /** 202: in flight, or its durability is unresolved. */
  | { readonly kind: 'pending' }
  /** Typed 409: refused before any effect, for this reason. */
  | { readonly kind: 'refused'; readonly reason: GoalRefusalReason }
  /** Empty 409: this request id names a different request. A page error; no effect. */
  | { readonly kind: 'conflict' }
  /** Empty 403: this request was refused before any effect. */
  | { readonly kind: 'forbidden' }
  /** 400 or 413: this request itself was refused before any effect. */
  | { readonly kind: 'rejected'; readonly status: number }
  /** 503: the daemon could not establish durability. */
  | { readonly kind: 'unavailable' }
  /** A lost response, an unexpected status or an unreadable body. */
  | { readonly kind: 'unknown' };

export interface GoalState {
  /** Unsent text, keyed by the agent and the goal it would replace. Module memory only. */
  readonly drafts: ReadonlyMap<string, string>;
  /** Unsent check choices, keyed as `drafts`. Module memory only. */
  readonly checkChoices: ReadonlyMap<string, CheckChoice>;
  /** The one request in flight or awaiting an outcome. Module memory only. */
  readonly attempt: GoalAttempt | null;
  readonly dismissedAttempt: GoalAttempt | null;
  readonly sending: boolean;
  readonly outcome: GoalOutcome | null;
  /** The goals, by agent and goal, whose change the daemon refused as `goal_exhausted`. Module memory only. */
  readonly exhausted: ReadonlySet<string>;
  /**
   * Whether any response to this request left its effect unknown. Once it
   * has, a later refusal proves only that the later request did nothing.
   */
  readonly uncertain: boolean;
  /** The floor epoch when a 403 asked for a fresh snapshot. */
  readonly askedAtEpoch: number;
  /** The session a 403 was received under, by `floorSessionGeneration()`. */
  readonly refusedUnder: number | null;
  /** A reason nothing was sent that no field owns. */
  readonly problem: string | null;
}

export interface GoalResponse {
  readonly status: number;
  text(): Promise<string>;
}

export interface GoalDeps {
  fetch(url: string, init: RequestInit): Promise<GoalResponse>;
  /** The session's CSRF nonce, or `null` when the page has no session. */
  csrf(): string | null;
  /** Fill `bytes` with cryptographically random values. */
  random(bytes: Uint8Array<ArrayBuffer>): void;
  /** Ask the stream for a fresh snapshot of this session. */
  refresh(): void;
  /** The floor's current subscription epoch. */
  epoch(): number;
  /** Which session the page holds now: `floorSessionGeneration()`. */
  session(): number;
}

export const browserGoalDeps: GoalDeps = {
  fetch: floorFetch,
  csrf: () => currentFloorSession()?.csrf ?? null,
  random: (bytes) => {
    crypto.getRandomValues(bytes);
  },
  refresh: () => {
    refreshFloorSnapshot();
  },
  epoch: () => getFloorState().epoch,
  session: () => floorSessionGeneration(),
};

const INITIAL: GoalState = Object.freeze({
  drafts: new Map<string, string>(), checkChoices: new Map<string, CheckChoice>(), attempt: null,
  dismissedAttempt: null, sending: false, outcome: null, exhausted: new Set<string>(), uncertain: false,
  askedAtEpoch: 0, refusedUnder: null, problem: null,
});

let state: GoalState = INITIAL;
const listeners = new Set<() => void>();

export function getGoalState(): GoalState {
  return state;
}

export function subscribeToGoal(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

/** What a reload does: forget everything. For tests; the page gets a fresh module per load. */
export function resetGoal(): void {
  state = Object.freeze({ ...INITIAL, drafts: new Map<string, string>(),
    checkChoices: new Map<string, CheckChoice>(), exhausted: new Set<string>() });
  listeners.clear();
}

function commit(next: GoalState): void {
  state = Object.freeze(next);
  for (const listener of [...listeners]) listener();
}

function draftKey(agent: string, goal: string): string {
  return `${agent} ${goal}`;
}

/** The unsent text that would replace one agent's goal. */
export function draftFor(goal: GoalState, agent: string, goalId: string, initial = ''): string {
  return goal.drafts.get(draftKey(agent, goalId)) ?? initial;
}

export function editGoalDraft(agent: string, goalId: string, text: string): void {
  const drafts = new Map(state.drafts);
  drafts.set(draftKey(agent, goalId), text);
  commit({ ...state, drafts, problem: null });
}

export function chooseGoalCheck(agent: string, goalId: string, choice: CheckChoice): void {
  const checkChoices = new Map(state.checkChoices);
  checkChoices.set(draftKey(agent, goalId), choice);
  commit({ ...state, checkChoices, problem: null });
}

/**
 * The check a change of this goal would run, or `null` while none is chosen:
 * an achieved goal starts with none, and a check the workspace no longer
 * offers counts as none.
 */
export function chosenGoalCheck(goal: GoalState, gate: OpenGoalGate): 'keep' | EnrolledCheck | null {
  const choice = goal.checkChoices.get(draftKey(gate.agent, gate.goal)) ?? (gate.achieved ? null : 'keep');
  if (choice === null || choice === 'keep') return choice;
  return gate.enrolled.find((check) => check.id === choice.id) ?? null;
}

const LONE_SURROGATE = /\p{Cs}/u;

/**
 * Why this text cannot be sent, or `null` when it can. Measured the daemon's
 * way: in UTF-8 bytes, after trimming only ASCII space, tab, CR and LF. Other
 * control characters are text here; only NUL is refused.
 */
export function problemWith(text: string, maxTextBytes: number): string | null {
  const trimmed = trimAsciiSpace(text);
  const bytes = utf8Bytes(trimmed);
  if (bytes === 0) return 'The goal is empty.';
  if (bytes > maxTextBytes) return `The goal is ${bytes} UTF-8 bytes; the limit is ${maxTextBytes}.`;
  if (trimmed.includes('\u0000')) return 'The goal carries a NUL character, which the daemon refuses.';
  if (LONE_SURROGATE.test(trimmed)) return 'The goal carries half of a surrogate pair, which is not text.';
  return null;
}

/**
 * The release gate: whether this request no longer holds the form back.
 *
 * Only when the daemon has confirmed that it had no effect: a typed refusal,
 * an empty 409, or a 403, 400 or 413 answering a request whose effect was
 * never in doubt. A 403 that follows `pending`, a 503 or a lost response
 * proves only that the later request did nothing. The earlier one may be in
 * the journal, so the request is kept for Check status from a fresh session.
 */
export function releasable(goal: GoalState): boolean {
  const outcome = goal.outcome;
  if (goal.sending || goal.attempt === null || outcome === null) return false;
  switch (outcome.kind) {
    case 'refused':
    case 'conflict':
      return true;
    case 'forbidden':
    case 'rejected':
      return !goal.uncertain;
    default:
      return false;
  }
}

/** Whether this request presented the given agent's given goal. */
export function presents(attempt: GoalAttempt, agent: string, goalId: string): boolean {
  return attempt.payload.agent === agent && attempt.payload.goal === goalId;
}

/**
 * Whether the form for one agent's goal must wait.
 *
 * Every form waits while a request is out and while one is unresolved. A 403
 * that settled its request holds every form while the page still holds the
 * session that was refused. A durable change holds back only the form for the
 * goal it replaced, until the projection shows the agent's goal moving.
 */
export function blocksForm(goal: GoalState, agent: string, goalId: string, session: number): boolean {
  if (goal.sending) return true;
  const { attempt, outcome } = goal;
  if (attempt === null) return false;
  if (outcome?.kind === 'changed') return presents(attempt, agent, goalId);
  if (outcome?.kind === 'forbidden' && !goal.uncertain) return goal.refusedUnder === session;
  return !releasable(goal);
}

/** Whether Check status may ask again: a request not let go, not settled, not in flight. */
export function checkable(goal: GoalState): boolean {
  return goal.attempt !== null && !goal.sending && goal.outcome !== null
    && goal.outcome.kind !== 'changed' && !releasable(goal);
}

/** Send. It refuses while a turn runs and while any change is out or unresolved, so a second press does nothing. */
export async function submitGoal(gate: GoalGate, deps: GoalDeps): Promise<void> {
  if (!gate.open || gate.turnRunning || blocksForm(state, gate.agent, gate.goal, deps.session())) return;
  const offer = gate.renew;
  const text = draftFor(state, gate.agent, gate.goal, offer?.text);
  const check = chosenGoalCheck(state, gate);
  if (problemWith(text, gate.maxTextBytes) !== null || check === null) return;
  const csrf = deps.csrf();
  if (csrf === null) {
    commit({ ...state, problem: 'This page has no Floor session, so nothing was sent.' });
    return;
  }
  const change: ChangePayload = {
    request_id: mintAttemptId(deps.random),
    agent: gate.agent,
    goal: gate.goal,
    text: trimAsciiSpace(text),
    ...(check === 'keep' ? {} : { check_id: check.id, check_revision: check.revision }),
  };
  const payload: GoalPayload = Object.freeze(offer === undefined ? change
    : { ...change, hop_cap: offer.hopCap, goal_usd_micros: offer.usd, goal_tokens: offer.tokens });
  const attempt: GoalAttempt = Object.freeze({ payload, body: JSON.stringify(payload), turnsAtSend: gate.turns,
    checkLabel: check === 'keep' ? gate.currentCheck : check.label, enrolled: gate.enrolled.length > 0 });
  commit({ ...state, attempt, sending: true, outcome: null, uncertain: false, askedAtEpoch: 0, refusedUnder: null,
    problem: null });
  await send(attempt, csrf, deps);
}

/** Check status: the same request, the same bytes, sent again by the operator. */
export async function checkGoalStatus(deps: GoalDeps): Promise<void> {
  const attempt = state.attempt;
  if (attempt === null || !checkable(state)) return;
  const csrf = deps.csrf();
  if (csrf === null) {
    commit({ ...state, problem: 'This page has no Floor session, so nothing was sent.' });
    return;
  }
  commit({ ...state, sending: true, problem: null });
  await send(attempt, csrf, deps);
}

function without<V>(map: ReadonlyMap<string, V>, key: string): ReadonlyMap<string, V> {
  const copy = new Map(map);
  copy.delete(key);
  return copy;
}

const UNSETTLED: ReadonlySet<GoalOutcome['kind']> = new Set(['pending', 'unavailable', 'unknown']);

async function send(attempt: GoalAttempt, csrf: string, deps: GoalDeps): Promise<void> {
  const outcome = await postGoal(attempt, csrf, deps.fetch);
  // The staleness guard: a reply to a request this page no longer holds is
  // not this page's to show.
  if (state.attempt !== attempt) return;
  const uncertain = state.uncertain || UNSETTLED.has(outcome.kind);
  let { drafts, checkChoices, exhausted } = state;
  if (outcome.kind === 'refused' && outcome.reason === 'goal_exhausted') {
    exhausted = new Set(exhausted).add(draftKey(attempt.payload.agent, attempt.payload.goal));
  }
  if (outcome.kind === 'changed') {
    const key = draftKey(attempt.payload.agent, attempt.payload.goal);
    drafts = without(drafts, key);
    checkChoices = without(checkChoices, key);
  }
  // An empty 403 may mean the session ended or that this page can no longer
  // act with it; a fresh subscription tells the two apart.
  const ask = outcome.kind === 'forbidden';
  commit({ ...state, drafts, checkChoices, exhausted, sending: false, outcome, uncertain,
    askedAtEpoch: ask ? deps.epoch() : state.askedAtEpoch, refusedUnder: ask ? deps.session() : state.refusedUnder });
  // A stale goal, a moved allowance or a changed check means the page's view is behind the daemon: read it again.
  if (ask || (outcome.kind === 'refused' && (outcome.reason === 'stale_goal' || outcome.reason === 'profile_changed'))) {
    deps.refresh();
  }
}

/** Send one request's body and read what the answer proves. */
export async function postGoal(attempt: GoalAttempt, csrf: string, fetch: GoalDeps['fetch']): Promise<GoalOutcome> {
  let status: number;
  let text: string;
  try {
    const response = await fetch(routeOf(attempt.payload), {
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
    return UNKNOWN;
  }
  return decodeGoalResponse(status, text, attempt.payload);
}

const UNKNOWN: GoalOutcome = Object.freeze({ kind: 'unknown' });

function exactly(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const own = Object.keys(value);
  return own.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function json(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

/**
 * The response map. Anything that does not match its row exactly proves
 * nothing, and that includes a 200 naming the goal it was asked to replace.
 */
export function decodeGoalResponse(status: number, text: string, payload: GoalPayload): GoalOutcome {
  switch (status) {
    case 200: {
      const body = json(text);
      return exactly(body, ['request_id', 'state', 'agent', 'goal'])
        && body.request_id === payload.request_id && body.state === (isRenewal(payload) ? 'renewed' : 'changed')
        && body.agent === payload.agent
        && typeof body.goal === 'string' && GOAL_ID.test(body.goal) && body.goal !== payload.goal
        ? Object.freeze({ kind: 'changed' }) : UNKNOWN;
    }
    case 202: {
      const body = json(text);
      return exactly(body, ['request_id', 'state']) && body.request_id === payload.request_id
        && body.state === 'pending' ? Object.freeze({ kind: 'pending' }) : UNKNOWN;
    }
    case 403:
      return Object.freeze({ kind: 'forbidden' });
    case 409: {
      if (text === '') return Object.freeze({ kind: 'conflict' });
      const body = json(text);
      if (!exactly(body, ['request_id', 'state', 'reason']) || body.request_id !== payload.request_id
        || body.state !== 'refused' || typeof body.reason !== 'string'
        || !((isRenewal(payload) ? RENEW_REASONS : CHANGE_REASONS).has(body.reason) || isCheckRefusal(body.reason))) {
        return UNKNOWN;
      }
      return Object.freeze({ kind: 'refused', reason: body.reason as GoalRefusalReason });
    }
    case 503:
      return Object.freeze({ kind: 'unavailable' });
    case 400:
    case 413:
      return Object.freeze({ kind: 'rejected', status });
    default:
      return UNKNOWN;
  }
}

/**
 * What a 403 means now. `checking` until a fresh snapshot or a closed stream
 * says; `ended` when the stream has given up; `live` when a fresh snapshot
 * arrived on the session that was refused, which can no longer act; and
 * `renewed` when the page has since signed in again and a fresh snapshot of
 * that new session offers goal change.
 */
export function afterRefusal(goal: GoalState, floor: FloorState, session: number):
  'checking' | 'ended' | 'live' | 'renewed' {
  const renewed = goal.refusedUnder !== null && session !== goal.refusedUnder;
  if (floor.phase === 'live' && floor.snapshotEpoch === floor.epoch && floor.snapshotEpoch > goal.askedAtEpoch) {
    return renewed && floor.goalChange ? 'renewed' : 'live';
  }
  if (floor.phase === 'closed') return 'ended';
  return 'checking';
}

export type Continuation =
  /** The page has no snapshot to read the agent from yet. */
  | { readonly kind: 'unconfirmed' }
  /** A current snapshot does not list the agent: it has left the floor. */
  | { readonly kind: 'absent' }
  /** The floor still shows the goal this request replaced. */
  | { readonly kind: 'unchanged' }
  /** The goal moved, and a pause holds the agent. */
  | { readonly kind: 'paused' }
  /** The goal moved, and a person is attached to the agent. */
  | { readonly kind: 'attached' }
  /** The goal moved, and no new turn has appeared yet. */
  | { readonly kind: 'waiting'; readonly status: string }
  /** The goal moved, and the agent has taken a new turn. */
  | { readonly kind: 'continued'; readonly from: number; readonly to: number; readonly status: string };

/** What the agent's goal and hold are after a durable change, read from the projection and nothing else. */
export function continuationOf(attempt: GoalAttempt, floor: FloorState): Continuation {
  const row = floor.agents.find((agent) => agent.id === attempt.payload.agent);
  const current = floor.phase === 'live' && floor.snapshotEpoch === floor.epoch;
  if (row === undefined) return Object.freeze({ kind: current ? 'absent' : 'unconfirmed' });
  if (goalIdOf(row) === attempt.payload.goal) return Object.freeze({ kind: 'unchanged' });
  if (row.paused === true) return Object.freeze({ kind: 'paused' });
  if (row.mode === 'attached') return Object.freeze({ kind: 'attached' });
  const status = typeof row.status === 'string' ? row.status : '';
  const turns = typeof row.turns === 'number' ? row.turns : 0;
  if (turns > attempt.turnsAtSend) {
    return Object.freeze({ kind: 'continued', from: attempt.turnsAtSend, to: turns, status });
  }
  return Object.freeze({ kind: 'waiting', status });
}

export function receiptDismissible(goal: GoalState): boolean {
  return goal.attempt !== null && !goal.sending
    && goal.outcome?.kind === 'changed';
}

export function dismissReceipt(): void {
  if (receiptDismissible(state)) commit({ ...state, dismissedAttempt: state.attempt });
}
