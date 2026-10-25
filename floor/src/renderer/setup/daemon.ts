// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the daemon reports about its own setup, read from the optional `daemon`
// sibling of a fresh `floor.snapshot`: for each provider it can hire, whether
// it finds that provider's program on its own PATH, and where its state lives.
//
// The sibling is display metadata. It grants nothing, and the page offers no
// install or edit control: an install command is text the operator copies and
// runs. So this reader fails closed. Anything absent or malformed
// reads as "not reported", and it never closes the stream.

/** The provider wire names, in the daemon's order. */
export const PROVIDERS = Object.freeze([
  'claude', 'codex', 'copilot', 'antigravity', 'grok', 'kimi', 'qwen', 'opencode', 'pi', 'crush',
] as const);

export type ProviderName = (typeof PROVIDERS)[number];

/** What the engine's own status command said: signed in, signed out, or nothing the daemon can use. */
export type SignInState = 'signed_in' | 'signed_out' | 'unknown';

/** One provider's row: the program the daemon looks for, the file it found, if any, and whether it is signed in. */
export interface ProviderRow {
  readonly provider: ProviderName;
  readonly program: string | null;
  readonly found: boolean;
  readonly path: string | null;
  readonly detail: string | null;
  /** `null` when the daemon did not say, which is not the same as `unknown`. */
  readonly signIn: SignInState | null;
  /** The model the operator's configuration fixes for this engine's agents, when it does. */
  readonly configuredModel?: string;
  /** Present when the configuration fixes the model in a way a hire cannot replace. */
  readonly modelFixed?: true;
  /** Present when the daemon has not confirmed that this engine continues a session under another model. */
  readonly modelChangeUnsupported?: true;
  /** What the engine offered, when the daemon holds a list. Never checked against the account behind the engine. */
  readonly models?: ModelListView;
}

/** How a listed model came to be listed. */
export type ModelItemKind = 'listed' | 'alias' | 'seen';

export interface ModelItem {
  readonly id: string;
  readonly kind: ModelItemKind;
}

export interface ModelListView {
  /** Milliseconds since the Unix epoch when the daemon made the list. */
  readonly asOf: number;
  readonly items: readonly ModelItem[];
}

export interface DaemonReport {
  readonly stateDir: string | null;
  readonly providers: readonly ProviderRow[];
}

/**
 * The install command the page shows for a provider it cannot find, as text
 * the operator runs. A constant of this bundle, never a wire field.
 */
export const INSTALL_COMMANDS: Readonly<Partial<Record<ProviderName, string>>> = Object.freeze({
  claude: 'npm install -g @anthropic-ai/claude-code',
  codex: 'npm install -g @openai/codex',
});

const MAX_PROGRAM_BYTES = 255;
const MAX_PATH_BYTES = 1_024;
const MAX_DETAIL_BYTES = 2_048;
const MAX_MODEL_BYTES = 256;
const CONTROL = /\p{Cc}/u;
const UTF8 = new TextEncoder();
const REPORT_KEYS = Object.freeze(['state_dir', 'providers']);
const ROW_KEYS = Object.freeze(['provider', 'program', 'found', 'path', 'detail', 'sign_in', 'configured_model', 'model_fixed',
  'model_change_unsupported', 'models']);
const MODEL_ID = /^[A-Za-z0-9._:/@[\]][A-Za-z0-9._:/@[\]-]{0,63}$/;
const MAX_MODEL_ITEMS = 200;
const ITEM_KINDS: readonly ModelItemKind[] = Object.freeze(['listed', 'alias', 'seen']);
const SIGN_IN_STATES: readonly SignInState[] = Object.freeze(['signed_in', 'signed_out', 'unknown']);

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function within(value: Record<string, unknown>, keys: readonly string[]): boolean {
  return Object.keys(value).every((key) => keys.includes(key));
}

/** An optional display string of 1 to `max` UTF-8 bytes with no control character. */
function text(value: unknown, max: number): string | null | undefined {
  if (value === undefined) return null;
  if (typeof value !== 'string' || CONTROL.test(value)) return undefined;
  const bytes = UTF8.encode(value).length;
  return bytes >= 1 && bytes <= max ? value : undefined;
}

/** The model list of a row, or `undefined` when it is absent or malformed. A bad list never costs the row. */
function modelList(value: unknown): ModelListView | undefined {
  if (!object(value) || !within(value, ['as_of', 'checked', 'items']) || value.checked !== false
    || typeof value.as_of !== 'number' || !Number.isSafeInteger(value.as_of) || value.as_of < 0
    || !Array.isArray(value.items) || value.items.length > MAX_MODEL_ITEMS) return undefined;
  const items: ModelItem[] = [];
  const seen = new Set<string>();
  for (const item of value.items as unknown[]) {
    if (!object(item) || !within(item, ['id', 'kind']) || typeof item.id !== 'string' || !MODEL_ID.test(item.id)
      || !ITEM_KINDS.some((kind) => kind === item.kind) || seen.has(item.id)) return undefined;
    seen.add(item.id);
    items.push(Object.freeze({ id: item.id, kind: item.kind as ModelItemKind }));
  }
  return Object.freeze({ asOf: value.as_of, items: Object.freeze(items) });
}

function row(value: unknown): ProviderRow | null {
  if (!object(value) || !within(value, ROW_KEYS)) return null;
  const provider = PROVIDERS.find((name) => name === value.provider);
  const program = text(value.program, MAX_PROGRAM_BYTES);
  const path = text(value.path, MAX_PATH_BYTES);
  const detail = text(value.detail, MAX_DETAIL_BYTES);
  const signIn = value.sign_in === undefined ? null : SIGN_IN_STATES.find((state) => state === value.sign_in);
  if (signIn === undefined || (!value.found && signIn !== null)) return null;
  if (provider === undefined || typeof value.found !== 'boolean' || program === undefined || path === undefined
    || detail === undefined || (!value.found && path !== null) || (value.found && detail !== null)) {
    return null;
  }
  const configured = value.configured_model === undefined ? null : text(value.configured_model, MAX_MODEL_BYTES);
  if (configured === undefined || (value.model_fixed !== undefined && value.model_fixed !== true)
    || (value.model_change_unsupported !== undefined && value.model_change_unsupported !== true)) return null;
  const models = value.models === undefined ? undefined : modelList(value.models);
  return Object.freeze({ provider, program, found: value.found, path, detail, signIn,
    ...(models === undefined ? {} : { models }),
    ...(configured === null ? {} : { configuredModel: configured }),
    ...(value.model_fixed === true ? { modelFixed: true as const } : {}),
    ...(value.model_change_unsupported === true ? { modelChangeUnsupported: true as const } : {}) });
}

/**
 * Read the optional `daemon` sibling of one snapshot.
 *
 * @returns the report, or `null` for absent or malformed data. It never throws.
 */
export function decodeDaemonReport(value: unknown): DaemonReport | null {
  if (!object(value) || !within(value, REPORT_KEYS) || !Array.isArray(value.providers)
    || value.providers.length > PROVIDERS.length) {
    return null;
  }
  const stateDir = text(value.state_dir, MAX_PATH_BYTES);
  const providers = value.providers.map(row);
  if (stateDir === undefined || providers.some((entry) => entry === null)) return null;
  const rows = providers as ProviderRow[];
  if (new Set(rows.map((entry) => entry.provider)).size !== rows.length) return null;
  return Object.freeze({ stateDir, providers: Object.freeze(rows) });
}

/** The row for `provider` in the latest report, or `null` when it has none. */
export function rowFor(report: DaemonReport | null, provider: string): ProviderRow | null {
  return report?.providers.find((entry) => entry.provider === provider) ?? null;
}
