// SPDX-License-Identifier: MIT OR Apache-2.0

import { validReleaseVersion } from './version.ts';
import { PROVIDERS } from '../setup/daemon.ts';
import type { ProviderName } from '../setup/daemon.ts';

export interface SettingsWorkspace {
  readonly workspace_id: string;
  readonly label: string;
  readonly agent_usd_per_day_micros: string | null;
  readonly agent_tokens_per_day: string | null;
  readonly goal_usd_micros: string | null;
  readonly goal_tokens: string | null;
}

export interface SettingsReport {
  readonly switches: Readonly<Record<(typeof SWITCHES)[number], boolean>>;
  readonly budget: {
    readonly floor_usd_per_day_micros: string | null;
    readonly per_turn_usd_micros: string;
    readonly per_turn_tokens: string;
  };
  readonly providers: readonly ProviderName[];
  readonly workspaces: readonly SettingsWorkspace[];
  readonly about: { readonly daemon_version: string; readonly browser_wire_version: 3 };
}

const UTF8 = new TextEncoder();
const DECIMAL = /^(?:0|[1-9][0-9]{0,19})$/;
const SWITCHES = ['answer', 'control', 'recovery', 'goal', 'retire', 'terminal', 'hire', 'git', 'renew'] as const;
const WORKSPACE_KEYS = ['workspace_id', 'label', 'agent_usd_per_day_micros', 'agent_tokens_per_day',
  'goal_usd_micros', 'goal_tokens'];

function object(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function exact(value: unknown, keys: readonly string[]): value is Record<string, unknown> {
  return object(value) && Object.keys(value).length === keys.length && keys.every((key) => Object.hasOwn(value, key));
}

function decimal(value: unknown): value is string {
  return typeof value === 'string' && DECIMAL.test(value) && BigInt(value) <= 18_446_744_073_709_551_615n;
}

function ceiling(value: unknown): value is string | null { return value === null || decimal(value); }

function workspace(value: unknown): SettingsWorkspace | null {
  if (!exact(value, WORKSPACE_KEYS) || typeof value.workspace_id !== 'string'
    || !/^[A-Za-z0-9_-]{1,128}$/.test(value.workspace_id) || typeof value.label !== 'string'
    || value.label.length === 0 || UTF8.encode(value.label).length > 128 || /\p{Cc}/u.test(value.label)
    || !ceiling(value.agent_usd_per_day_micros) || !ceiling(value.agent_tokens_per_day)
    || !ceiling(value.goal_usd_micros) || !ceiling(value.goal_tokens)) return null;
  return Object.freeze({ workspace_id: value.workspace_id, label: value.label,
    agent_usd_per_day_micros: value.agent_usd_per_day_micros, agent_tokens_per_day: value.agent_tokens_per_day,
    goal_usd_micros: value.goal_usd_micros, goal_tokens: value.goal_tokens });
}

export function decodeSettingsReport(value: unknown): SettingsReport | null {
  if (!exact(value, ['switches', 'budget', 'providers', 'workspaces', 'about'])
    || !exact(value.switches, SWITCHES) || Object.values(value.switches).some((enabled) => typeof enabled !== 'boolean')
    || !exact(value.budget, ['floor_usd_per_day_micros', 'per_turn_usd_micros', 'per_turn_tokens'])
    || !ceiling(value.budget.floor_usd_per_day_micros) || !decimal(value.budget.per_turn_usd_micros)
    || !decimal(value.budget.per_turn_tokens) || !Array.isArray(value.providers)
    || value.providers.length > PROVIDERS.length || new Set(value.providers).size !== value.providers.length
    || value.providers.some((provider) => !PROVIDERS.some((name) => name === provider))
    || !Array.isArray(value.workspaces) || value.workspaces.length > 32
    || !exact(value.about, ['daemon_version', 'browser_wire_version'])
    || !validReleaseVersion(value.about.daemon_version) || value.about.browser_wire_version !== 3
    || UTF8.encode(JSON.stringify(value)).length > 32_768) return null;
  const rows = value.workspaces.map(workspace);
  if (rows.some((row) => row === null)) return null;
  const workspaces = rows as SettingsWorkspace[];
  if (new Set(workspaces.map((row) => row.workspace_id)).size !== workspaces.length) return null;
  const switches = value.switches as unknown as SettingsReport['switches'];
  return Object.freeze({
    switches: Object.freeze({ answer: switches.answer, control: switches.control, recovery: switches.recovery, goal: switches.goal,
      retire: switches.retire, terminal: switches.terminal, hire: switches.hire, git: switches.git, renew: switches.renew }),
    budget: Object.freeze({ floor_usd_per_day_micros: value.budget.floor_usd_per_day_micros,
      per_turn_usd_micros: value.budget.per_turn_usd_micros, per_turn_tokens: value.budget.per_turn_tokens }),
    providers: Object.freeze([...value.providers] as ProviderName[]), workspaces: Object.freeze(workspaces),
    about: Object.freeze({ daemon_version: value.about.daemon_version, browser_wire_version: 3 }),
  });
}
