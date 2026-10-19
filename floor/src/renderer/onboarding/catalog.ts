// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The enrolled choices a Hire may offer, as the daemon states them.
//
// They ride one optional `onboarding` field beside `projection` on a
// `floor.snapshot`. The field is display metadata, never authority: the Hire
// POST re-checks everything, and a snapshot promises nothing about a later
// Hire. So this reader fails closed. Anything absent, malformed or over a bound
// is "Hire unavailable", and a catalog is taken whole or not at all — never
// trimmed down to the entries that happen to parse.
//
// Ceilings arrive as decimal strings, micro-dollars or tokens, because a `u64`
// does not fit in a JavaScript number, and `null` for a ceiling the operator
// left unset. They stay strings here and are formatted by string arithmetic
// alone: a ceiling passed through `Number` would be shown rounded, which is a
// different ceiling from the one the daemon enforces.

/** The engines a browser Hire may run, in the daemon's order, by display name. */
export const HIRE_ENGINES = Object.freeze({ claude: 'Claude', codex: 'Codex', pi: 'Pi' } as const);

export type HireEngine = keyof typeof HIRE_ENGINES;

export function isHireEngine(value: unknown): value is HireEngine {
  return typeof value === 'string' && Object.hasOwn(HIRE_ENGINES, value);
}

/**
 * What an operator reload last did to a workspace in this daemon run. A Hire
 * of that workspace must present `revision`, so a re-point that lands after
 * this page was offered the entry cannot redirect the Hire.
 */
export interface ReloadMark {
  readonly kind: 'added' | 'changed';
  /** The daemon's UTC time of that reload, RFC 3339 to the second. */
  readonly at: string;
  /** A canonical decimal string. */
  readonly revision: string;
}

/** One operator-enrolled workspace, exactly as the daemon lists it. */
export interface EnrolledWorkspace {
  readonly workspaceId: string;
  readonly label: string;
  /** The engines offered for this project: at least one, in `HIRE_ENGINES` order. */
  readonly engines: readonly HireEngine[];
  /** Each ceiling is `null` when unset, which is no cap. */
  readonly goalUsdMicros: string | null;
  readonly agentUsdPerDayMicros: string | null;
  readonly agentTokensPerDay: string | null;
  readonly goalTokens: string | null;
  readonly hopCap: number;
  /** Present only for a workspace a reload added or changed. */
  readonly reloadMark?: ReloadMark;
}

/** The whole offer: the floor-wide ceiling and every eligible workspace. */
export interface OnboardingCatalog {
  readonly floorUsdPerDayMicros: string | null;
  readonly workspaces: readonly EnrolledWorkspace[];
}

const MAX_WORKSPACES = 32;
const MAX_SERIALIZED_BYTES = 16_384;
const MAX_DISPLAY_BYTES = 128;
const WORKSPACE_ID = /^[A-Za-z0-9_-]{1,128}$/;
const DECIMAL = /^(?:0|[1-9][0-9]{0,19})$/;
const U64_MAX = '18446744073709551615';
const U32_MAX = 4_294_967_295;
const CONTROL = /\p{Cc}/u;
const RFC3339_SECONDS = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/;
const ENGINE_ORDER = Object.freeze(Object.keys(HIRE_ENGINES) as HireEngine[]);
const MARK_KEYS = Object.freeze(['kind', 'at', 'revision']);
const UTF8 = new TextEncoder();

const CATALOG_KEYS = Object.freeze(['floor_usd_per_day_micros', 'workspaces']);
const WORKSPACE_KEYS = Object.freeze([
  'workspace_id', 'label', 'engines', 'goal_usd_micros', 'agent_usd_per_day_micros', 'agent_tokens_per_day',
  'goal_tokens', 'hop_cap',
]);
const MARKED_WORKSPACE_KEYS = Object.freeze([...WORKSPACE_KEYS, 'reload_mark']);

function exactly(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const own = Object.keys(value);
  return own.length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

/** A canonical decimal `u64`, `"0"` included. */
function u64(value: unknown): string | null {
  if (typeof value !== 'string' || !DECIMAL.test(value)) return null;
  // Same length, so string order is numeric order.
  if (value.length === U64_MAX.length && value > U64_MAX) return null;
  return value;
}

/**
 * A ceiling: `null` when unset, `undefined` when malformed. `"0"` is a set
 * ceiling that admits no spend; the daemon never sends it where `allowZero` is false.
 */
function ceiling(value: unknown, allowZero: boolean): string | null | undefined {
  if (value === null) return null;
  const amount = u64(value);
  return amount === null || (!allowZero && amount === '0') ? undefined : amount;
}

/** A non-empty list of offered engines, each once, in `HIRE_ENGINES` order. */
function engines(value: unknown): readonly HireEngine[] | null {
  if (!Array.isArray(value) || value.length === 0) return null;
  let previous = -1;
  for (const engine of value) {
    const at = ENGINE_ORDER.indexOf(engine as HireEngine);
    if (at <= previous) return null;
    previous = at;
  }
  return Object.freeze([...value] as HireEngine[]);
}

function reloadMark(value: unknown): ReloadMark | null {
  if (!exactly(value, MARK_KEYS)) return null;
  const { kind, at } = value;
  const revision = u64(value.revision);
  if ((kind !== 'added' && kind !== 'changed') || typeof at !== 'string' || !RFC3339_SECONDS.test(at)
    || revision === null) {
    return null;
  }
  return Object.freeze({ kind, at, revision });
}

function display(value: unknown): string | null {
  if (typeof value !== 'string' || CONTROL.test(value)) return null;
  const bytes = UTF8.encode(value).length;
  return bytes >= 1 && bytes <= MAX_DISPLAY_BYTES ? value : null;
}

/**
 * Read the optional `onboarding` sibling of one snapshot.
 *
 * @returns the catalog, or `null` for absent, malformed or over-bound data. It
 * never throws: an unreadable offer must leave the stream and the floor alone.
 */
export function decodeOnboardingCatalog(value: unknown): OnboardingCatalog | null {
  if (!exactly(value, CATALOG_KEYS)) return null;
  if (UTF8.encode(JSON.stringify(value)).length > MAX_SERIALIZED_BYTES) return null;
  const floor = ceiling(value.floor_usd_per_day_micros, true);
  const list = value.workspaces;
  if (floor === undefined || !Array.isArray(list) || list.length === 0 || list.length > MAX_WORKSPACES) {
    return null;
  }
  const workspaces: EnrolledWorkspace[] = [];
  for (const entry of list) {
    const marked = exactly(entry, MARKED_WORKSPACE_KEYS);
    if (!marked && !exactly(entry, WORKSPACE_KEYS)) return null;
    const mark = marked ? reloadMark(entry.reload_mark) : null;
    if (marked && mark === null) return null;
    const id = entry.workspace_id;
    const previous = workspaces.at(-1)?.workspaceId;
    if (typeof id !== 'string' || !WORKSPACE_ID.test(id) || (previous !== undefined && !(id > previous))) {
      return null;
    }
    const label = display(entry.label);
    const offered = engines(entry.engines);
    const goal = ceiling(entry.goal_usd_micros, false);
    const agent = ceiling(entry.agent_usd_per_day_micros, false);
    const agentTokens = ceiling(entry.agent_tokens_per_day, true);
    const goalTokens = ceiling(entry.goal_tokens, true);
    const hops = entry.hop_cap;
    if (label === null || offered === null || goal === undefined || agent === undefined
      || agentTokens === undefined || goalTokens === undefined
      || typeof hops !== 'number' || !Number.isInteger(hops) || hops < 1 || hops > U32_MAX) {
      return null;
    }
    workspaces.push(Object.freeze({
      workspaceId: id, label, engines: offered,
      goalUsdMicros: goal, agentUsdPerDayMicros: agent, agentTokensPerDay: agentTokens, goalTokens, hopCap: hops,
      ...(mark === null ? {} : { reloadMark: mark }),
    }));
  }
  return Object.freeze({ floorUsdPerDayMicros: floor, workspaces: Object.freeze(workspaces) });
}

/**
 * A micro-dollar string as dollars, exactly: `"18446744073709551615"` is
 * `$18,446,744,073,709.551615`. String arithmetic only.
 */
export function formatUsdMicros(value: string): string {
  if (!/^(?:0|[1-9][0-9]*)$/.test(value)) throw new TypeError('not a canonical micro-dollar amount');
  const padded = value.padStart(7, '0');
  const whole = padded.slice(0, -6).replace(/\B(?=(?:\d{3})+(?!\d))/g, ',');
  const fraction = padded.slice(-6).replace(/0+$/, '');
  return fraction === '' ? `$${whole}` : `$${whole}.${fraction}`;
}
