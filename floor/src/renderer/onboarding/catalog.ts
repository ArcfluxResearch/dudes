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
  /**
   * The operator trusts this project, so a Claude hire may work in its own copy of it. Nothing
   * else of the grant reaches the page. A daemon that does not say is read as not trusting.
   */
  readonly trusted: boolean;
  /** Present only for a workspace a reload added or changed. */
  readonly reloadMark?: ReloadMark;
  readonly repositoryGroup?: RepositoryGroupOffer;
}

export interface ProjectHireTarget {
  readonly project_id: string;
  readonly expected_revision: string;
  readonly primary_repo: string;
  readonly repo_ids: readonly string[];
}

export interface RepositoryGroupOffer {
  readonly project: ProjectHireTarget;
  readonly repositories: readonly { readonly repo_id: string; readonly label: string }[];
  readonly providers: readonly {
    readonly provider: HireEngine;
    readonly enabled: boolean;
    readonly model: string | null;
    readonly refusal: string | null;
  }[];
}

/** Whether the group offer fixes the model for `engine`; when it does not, the operator chooses as for any hire. */
export function groupPinsModel(group: RepositoryGroupOffer, engine: HireEngine): boolean {
  return group.providers.find((offer) => offer.provider === engine)?.model != null;
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
const OPTIONAL_WORKSPACE_KEYS = Object.freeze(['trusted', 'reload_mark', 'repository_group']);
const REPO_ID = /^[a-z][a-z0-9_-]{0,31}$/;

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
function engines(value: unknown, allowEmpty = false): readonly HireEngine[] | null {
  if (!Array.isArray(value) || (!allowEmpty && value.length === 0)) return null;
  let previous = -1;
  for (const engine of value) {
    const at = ENGINE_ORDER.indexOf(engine as HireEngine);
    if (at <= previous) return null;
    previous = at;
  }
  return Object.freeze([...value] as HireEngine[]);
}

export function decodeProjectHireTarget(value: unknown): ProjectHireTarget | null {
  if (!exactly(value, ['project_id', 'expected_revision', 'primary_repo', 'repo_ids'])
    || typeof value.project_id !== 'string' || !WORKSPACE_ID.test(value.project_id)
    || u64(value.expected_revision) === null || value.expected_revision === '0'
    || typeof value.primary_repo !== 'string' || !REPO_ID.test(value.primary_repo)
    || !Array.isArray(value.repo_ids) || value.repo_ids.length < 1 || value.repo_ids.length > 8
    || !value.repo_ids.every((id) => typeof id === 'string' && REPO_ID.test(id))
    || new Set(value.repo_ids).size !== value.repo_ids.length || !value.repo_ids.includes(value.primary_repo)) return null;
  return Object.freeze({ project_id: value.project_id, expected_revision: value.expected_revision as string,
    primary_repo: value.primary_repo, repo_ids: Object.freeze([...value.repo_ids] as string[]) });
}

function repositoryGroup(value: unknown): RepositoryGroupOffer | null {
  if (!exactly(value, ['project', 'repositories', 'providers'])) return null;
  const project = decodeProjectHireTarget(value.project);
  if (project === null || !Array.isArray(value.repositories) || value.repositories.length !== project.repo_ids.length
    || !Array.isArray(value.providers) || value.providers.length !== ENGINE_ORDER.length) return null;
  const repositories: { repo_id: string; label: string }[] = [];
  for (const [index, member] of value.repositories.entries()) {
    if (!exactly(member, ['repo_id', 'label']) || member.repo_id !== project.repo_ids[index]) return null;
    const label = display(member.label);
    if (label === null) return null;
    repositories.push(Object.freeze({ repo_id: project.repo_ids[index]!, label }));
  }
  const providers: RepositoryGroupOffer['providers'][number][] = [];
  for (const [index, offer] of value.providers.entries()) {
    if (!exactly(offer, ['provider', 'enabled', 'model', 'refusal']) || offer.provider !== ENGINE_ORDER[index]
      || typeof offer.enabled !== 'boolean'
      || (offer.model !== null && (typeof offer.model !== 'string' || offer.model.length < 1 || offer.model.length > 64))
      || (offer.enabled ? offer.refusal !== null : display(offer.refusal) === null)) return null;
    providers.push(Object.freeze({ provider: ENGINE_ORDER[index]!, enabled: offer.enabled,
      model: offer.model as string | null, refusal: offer.refusal as string | null }));
  }
  return Object.freeze({ project, repositories: Object.freeze(repositories), providers: Object.freeze(providers) });
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
    if (typeof entry !== 'object' || entry === null || Array.isArray(entry)) return null;
    const own = Object.keys(entry);
    if (!WORKSPACE_KEYS.every((key) => own.includes(key))
      || !own.every((key) => WORKSPACE_KEYS.includes(key) || OPTIONAL_WORKSPACE_KEYS.includes(key))) {
      return null;
    }
    const fields = entry as Record<string, unknown>;
    const marked = own.includes('reload_mark');
    const mark = marked ? reloadMark(fields.reload_mark) : null;
    if (marked && mark === null) return null;
    if (own.includes('trusted') && typeof fields.trusted !== 'boolean') return null;
    const id = fields.workspace_id;
    const previous = workspaces.at(-1)?.workspaceId;
    if (typeof id !== 'string' || !WORKSPACE_ID.test(id) || (previous !== undefined && !(id > previous))) {
      return null;
    }
    const label = display(fields.label);
    const group = own.includes('repository_group') ? repositoryGroup(fields.repository_group) : undefined;
    if (group === null || (group !== undefined && group.project.project_id !== id)) return null;
    const offered = engines(fields.engines, group !== undefined);
    if (group !== undefined && (offered === null
      || offered.join(',') !== group.providers.filter((offer) => offer.enabled).map((offer) => offer.provider).join(','))) return null;
    const goal = ceiling(fields.goal_usd_micros, false);
    const agent = ceiling(fields.agent_usd_per_day_micros, false);
    const agentTokens = ceiling(fields.agent_tokens_per_day, true);
    const goalTokens = ceiling(fields.goal_tokens, true);
    const hops = fields.hop_cap;
    if (label === null || offered === null || goal === undefined || agent === undefined
      || agentTokens === undefined || goalTokens === undefined
      || typeof hops !== 'number' || !Number.isInteger(hops) || hops < 1 || hops > U32_MAX) {
      return null;
    }
    workspaces.push(Object.freeze({
      workspaceId: id, label, engines: offered,
      goalUsdMicros: goal, agentUsdPerDayMicros: agent, agentTokensPerDay: agentTokens, goalTokens, hopCap: hops,
      trusted: fields.trusted === true,
      ...(mark === null ? {} : { reloadMark: mark }),
      ...(group === undefined ? {} : { repositoryGroup: group }),
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
