// SPDX-License-Identifier: MIT OR Apache-2.0

import { currentFloorSession, floorSessionGeneration } from '../../session.ts';
import { floorFetch } from '../../transport.ts';
import { getFloorState } from '../floor/store.ts';
import type { FloorState } from '../floor/store.ts';
import { goalIdOf } from '../goal/goal.ts';
import type { AgentRow } from '../scene/cast.ts';
import { advertisedChoices, decodeInspection, decodeReceipt, decodeSubmission } from './wire.ts';
import type { Inspection, Receipt, ReceiptRead, RecoveryChoice, RecoveryExpected, RecoveryWork, Submission } from './wire.ts';

type Available = Extract<Inspection, { readonly state: 'available' }>;
export interface Draft {
  readonly key: string;
  readonly sequence: number;
  readonly inspection: Available;
  readonly epoch: number;
  readonly session: number;
  readonly choice: RecoveryChoice | null;
  readonly requestId: string | null;
}
export type InspectionView = { readonly kind: 'loading' } | { readonly kind: 'draft'; readonly key: string }
  | { readonly kind: 'unavailable'; readonly reason: Extract<Inspection, { state: 'unavailable' }>['reason'] }
  | { readonly kind: 'problem'; readonly text: string };
export type SubmitOutcome = Submission | { readonly state: 'forbidden' };
export type ReadOutcome = ReceiptRead | { readonly state: 'unknown' | 'forbidden' };
export interface RecoveryRequest {
  readonly payload: { readonly request_id: string; readonly expected: RecoveryExpected; readonly choice: RecoveryChoice };
  readonly body: string;
  readonly name: string;
  readonly submitting: boolean;
  readonly outcome: SubmitOutcome | null;
  readonly checking: boolean;
  readonly read: ReadOutcome | null;
  readonly lastReceipt: Receipt | null;
  readonly settledAfterInspection: number;
  readonly absentAfterInspection: number;
  readonly finishedAfterInspection: number | null;
}
export interface RecoveryState {
  readonly inspections: ReadonlyMap<string, InspectionView>;
  readonly drafts: ReadonlyMap<string, Draft>;
  readonly requests: ReadonlyMap<string, RecoveryRequest>;
}
export interface RecoveryDeps {
  fetch(url: string, init: RequestInit): Promise<{ readonly status: number; text(): Promise<string> }>;
  csrf(): string | null;
  session(): number;
  floor(): FloorState;
  random(bytes: Uint8Array<ArrayBuffer>): void;
}
export const browserRecoveryDeps: RecoveryDeps = {
  fetch: floorFetch,
  csrf: () => currentFloorSession()?.csrf ?? null,
  session: floorSessionGeneration,
  floor: getFloorState,
  random: bytes => { crypto.getRandomValues(bytes); },
};
const INITIAL: RecoveryState = { inspections: new Map(), drafts: new Map(), requests: new Map() };
let state = INITIAL;
let inspectionNumber = 0;
const listeners = new Set<() => void>();
export function getRecoveryState(): RecoveryState { return state; }
export function subscribeToRecovery(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}
export function resetRecovery(): void { state = INITIAL; inspectionNumber = 0; listeners.clear(); }
function commit(next: RecoveryState): void { state = next; for (const listener of [...listeners]) listener(); }
function view(agent: string, value: InspectionView): void {
  commit({ ...state, inspections: new Map(state.inspections).set(agent, value) });
}
function updateRequest(id: string, change: Partial<RecoveryRequest>): void {
  const original = state.requests.get(id);
  if (original) commit({ ...state, requests: new Map(state.requests).set(id, Object.freeze({ ...original, ...change })) });
}
export function liveReader(floor: FloorState): boolean {
  return floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch;
}
export function recoveryEnabled(floor: FloorState, row: AgentRow | null): boolean {
  return liveReader(floor) && floor.recovery && floor.control && row !== null
    && row.mode === 'managed' && floor.agents.some(agent => agent.id === row.id) && goalIdOf(row) !== null;
}
export function draftProblem(draft: Draft, floor: FloorState, session: number): string | null {
  const row = floor.agents.find(agent => agent.id === draft.inspection.expected.work.agent) ?? null;
  if (!liveReader(floor)) return 'Recovery waits for a live, confirmed Floor view. Your choice and receipts are kept.';
  if (!floor.recovery || !floor.control) return 'Recovery needs floor.recovery and floor.control. Your choice and receipts are kept. Enable both locally, then restart the daemon.';
  if (session !== draft.session || floor.epoch !== draft.epoch) return 'This page has a different Floor session or snapshot. Inspect again and make a fresh choice. Nothing is sent automatically.';
  if (!recoveryEnabled(floor, row) || row === null || goalIdOf(row) !== draft.inspection.expected.work.goal
    || (row.control_lsn ?? null) !== draft.inspection.expected.control_lsn) {
    return 'The inspected target changed. Inspect again and make a fresh choice. The earlier request keeps its original target.';
  }
  return null;
}
export function requestReceipt(request: RecoveryRequest): Receipt | null {
  return request.lastReceipt;
}
export function blocksDraft(draft: Draft, memory: RecoveryState = state): boolean {
  if (draft.requestId !== null) return true;
  return [...memory.requests.values()].some(request => {
    if (!sameWork(request.payload.expected.work, draft.inspection.expected.work)) return false;
    if (draft.sequence <= request.absentAfterInspection) return true;
    if (inspectedSuccessor(request, draft)) return false;
    const receipt = requestReceipt(request);
    if (request.read?.state === 'absent' || request.outcome?.state === 'refused' || request.outcome?.state === 'forbidden'
      || receipt?.status.state === 'invalidated' || receipt?.status.state === 'superseded') return draft.sequence <= request.settledAfterInspection;
    if (receipt?.status.state === 'consumed') return draft.sequence <= request.settledAfterInspection
      || JSON.stringify(request.payload.expected) === JSON.stringify(draft.inspection.expected);
    return true;
  });
}
function inspectedSuccessor(request: RecoveryRequest, draft: Draft): boolean {
  if (request.submitting || request.finishedAfterInspection === null || draft.sequence <= request.finishedAfterInspection) return false;
  const next = draft.inspection.expected;
  if (next.episode === null || next.expected_predecessor !== null || !sameWork(next.episode.work, next.work)) return false;
  const previous = request.lastReceipt?.claim.episode
    ?? (request.payload.expected.expected_predecessor === null ? request.payload.expected.episode : null);
  return previous !== null && sameWork(previous.work, next.work) && previous.origin !== next.episode.origin;
}
function init(body: string, csrf: string): RequestInit {
  return { method: 'POST', credentials: 'same-origin', cache: 'no-store', referrerPolicy: 'no-referrer',
    headers: { 'Content-Type': 'application/json', 'X-Dudes-CSRF': csrf }, body };
}
function freeze<T>(value: T): T {
  if (typeof value === 'object' && value !== null) { for (const child of Object.values(value)) freeze(child); Object.freeze(value); }
  return value;
}
export async function inspectRecovery(agent: string, deps: RecoveryDeps): Promise<void> {
  const floor = deps.floor();
  const row = floor.agents.find(row => row.id === agent) ?? null;
  if (!recoveryEnabled(floor, row) || state.inspections.get(agent)?.kind === 'loading') return;
  const csrf = deps.csrf();
  if (csrf === null) { view(agent, { kind: 'problem', text: 'This page has no Floor session. Paste a new link from dudes floor open into this tab. Nothing was sent.' }); return; }
  const session = deps.session();
  const sequence = ++inspectionNumber;
  const marker: InspectionView = { kind: 'loading' };
  view(agent, marker);
  let result: Inspection | null = null;
  let forbidden = false;
  try {
    const response = await deps.fetch('/floor/recovery/inspect', init(JSON.stringify({ agent }), csrf));
    forbidden = response.status === 403;
    if (response.status === 200) result = decodeInspection(await response.text());
  } catch {}
  if (state.inspections.get(agent) !== marker) return;
  if (result === null) {
    view(agent, { kind: 'problem', text: forbidden ? 'This Floor session cannot inspect recovery. Paste a new link from dudes floor open into this tab, then inspect explicitly.'
      : 'Recovery could not be inspected. No choice was submitted. Inspect again when the daemon is available.' });
    return;
  }
  if (result.state === 'unavailable') { view(agent, { kind: 'unavailable', reason: result.reason }); return; }
  if (result.expected.work.agent !== agent || result.expected.work.goal !== (row === null ? null : goalIdOf(row))) {
    view(agent, { kind: 'problem', text: 'The inspected target does not match this agent and goal. No choice was submitted.' }); return;
  }
  const key = `${JSON.stringify(result.expected)}#${sequence}`;
  const draft: Draft = Object.freeze({ key, sequence, inspection: freeze(result), epoch: floor.epoch, session, choice: null, requestId: null });
  commit({ ...state, drafts: new Map(state.drafts).set(key, draft), inspections: new Map(state.inspections).set(agent, { kind: 'draft', key }) });
}
export function chooseRecovery(key: string, choice: RecoveryChoice): void {
  const draft = state.drafts.get(key);
  if (!draft || draft.requestId !== null || !advertisedChoices(draft.inspection.choices).includes(choice)) return;
  commit({ ...state, drafts: new Map(state.drafts).set(key, Object.freeze({ ...draft, choice })) });
}
export async function submitRecovery(key: string, deps: RecoveryDeps): Promise<void> {
  const draft = state.drafts.get(key);
  if (!draft || draft.choice === null || blocksDraft(draft) || draftProblem(draft, deps.floor(), deps.session()) !== null) return;
  const csrf = deps.csrf();
  if (csrf === null) return;
  const bytes = new Uint8Array(32);
  deps.random(bytes);
  const requestId = [...bytes].map(byte => byte.toString(16).padStart(2, '0')).join('');
  const payload = freeze({ request_id: requestId, expected: draft.inspection.expected, choice: draft.choice });
  const row = deps.floor().agents.find(row => row.id === payload.expected.work.agent);
  const request: RecoveryRequest = Object.freeze({ payload, body: JSON.stringify(payload), name: typeof row?.name === 'string' ? row.name : payload.expected.work.agent,
    submitting: true, outcome: null, checking: false, read: null, lastReceipt: null, settledAfterInspection: 0, absentAfterInspection: 0, finishedAfterInspection: null });
  commit({ ...state, drafts: new Map(state.drafts).set(key, Object.freeze({ ...draft, requestId: payload.request_id })),
    requests: new Map(state.requests).set(payload.request_id, request) });
  let outcome: SubmitOutcome = { state: 'unknown', request_id: payload.request_id };
  try {
    const response = await deps.fetch('/floor/recovery/submit', init(request.body, csrf));
    const decoded = decodeSubmission(response.status, await response.text());
    if (response.status === 403) outcome = { state: 'forbidden' };
    else if (decoded?.state === 'refused') outcome = decoded;
    else if (decoded?.state === 'unknown' && decoded.request_id === payload.request_id) outcome = decoded;
    else if (decoded?.state === 'accepted' && receiptMatches(decoded.receipt, request)) outcome = freeze(decoded);
  } catch {}
  const lastReceipt = outcome.state === 'accepted' ? outcome.receipt : null;
  updateRequest(payload.request_id, { submitting: false, outcome, lastReceipt, finishedAfterInspection: inspectionNumber,
    settledAfterInspection: outcome.state === 'refused' || outcome.state === 'forbidden' || closedReceipt(lastReceipt) ? inspectionNumber : 0 });
}
function closedReceipt(receipt: Receipt | null): boolean {
  return receipt !== null && (receipt.status.state === 'consumed' || receipt.status.state === 'invalidated' || receipt.status.state === 'superseded');
}
function receiptMatches(receipt: Receipt, request: RecoveryRequest): boolean {
  const expectedEpisode = request.payload.expected.episode;
  const known = requestReceipt(request);
  return receipt.request_id === request.payload.request_id && receipt.choice === request.payload.choice
    && sameWork(receipt.claim.episode.work, request.payload.expected.work)
    && (expectedEpisode === null || request.payload.expected.expected_predecessor !== null || receipt.claim.episode.origin === expectedEpisode.origin)
    && (known === null || receipt.claim.episode.origin === known.claim.episode.origin
      && receipt.claim.authorization_lsn === known.claim.authorization_lsn);
}
function sameWork(left: RecoveryWork, right: RecoveryWork): boolean {
  return left.agent === right.agent && left.goal === right.goal && left.turn === right.turn;
}
export async function checkRecoveryReceipt(id: string, deps: RecoveryDeps): Promise<void> {
  const request = state.requests.get(id);
  if (!request || request.submitting || request.checking || !liveReader(deps.floor())) return;
  const csrf = deps.csrf();
  if (csrf === null) { updateRequest(id, { read: { state: 'forbidden' } }); return; }
  updateRequest(id, { checking: true });
  let read: ReadOutcome = { state: 'unknown' };
  try {
    const response = await deps.fetch('/floor/recovery/receipt', init(JSON.stringify({ request_id: id }), csrf));
    if (response.status === 403) read = { state: 'forbidden' };
    else if (response.status === 200) {
      const decoded = decodeReceipt(await response.text());
      if (decoded !== null && (decoded.state !== 'found' || receiptMatches(decoded.receipt, request))) read = freeze(decoded);
    }
  } catch {}
  updateRequest(id, { checking: false, read, lastReceipt: read.state === 'found' ? read.receipt : request.lastReceipt,
    absentAfterInspection: read.state === 'absent' ? inspectionNumber : request.absentAfterInspection,
    settledAfterInspection: read.state === 'absent' || read.state === 'found' && closedReceipt(read.receipt)
      ? inspectionNumber : request.settledAfterInspection });
}
