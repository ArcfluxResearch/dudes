// SPDX-License-Identifier: MIT OR Apache-2.0

import type { FloorState } from './floor/store.ts';
import type { RetiredAgentRow } from './archive.ts';
import type { DetailTab } from './detailTabs.ts';
import { isLayoutId } from './office3d/offices.ts';
import type { LayoutId } from './office3d/offices.ts';

export type ThemePreference = 'system' | 'light' | 'dark';

export interface ViewPreferences {
  readonly theme: ThemePreference;
  readonly sidebarWidth: number;
  readonly rosterCollapsed: boolean;
}

export interface ActivityFilters {
  readonly agentId: string;
  readonly kind: string;
  readonly query: string;
}

export const DEFAULT_SIDEBAR_WIDTH = 376;
export const MIN_SIDEBAR_WIDTH = 288;
export const MAX_SIDEBAR_WIDTH = 560;
const SIDEBAR_STEP = 16;
const STORAGE_KEY = 'dudes.floor.view-preferences';
const GUIDE_CLOSED_STORAGE_KEY = 'dudes.floor.first-run-guide.closed';
const ACTIVITY_FILTERS_STORAGE_KEY = 'dudes.floor.activity-filters';
const OFFICE_PIN_STORAGE_KEY = 'dudes.floor.office-layout';
const ACTIVITY_KIND = /^[a-z_]{1,32}$/;
const MAX_ACTIVITY_QUERY = 128;

const DEFAULT_PREFERENCES: ViewPreferences = Object.freeze({
  theme: 'system', sidebarWidth: DEFAULT_SIDEBAR_WIDTH, rosterCollapsed: false,
});
const DEFAULT_ACTIVITY_FILTERS: ActivityFilters = Object.freeze({ agentId: '', kind: '', query: '' });

interface ViewStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

function browserStorage(): ViewStorage {
  return window.localStorage;
}

export function readGuideClosed(storage: () => ViewStorage = browserStorage): boolean {
  try { return storage().getItem(GUIDE_CLOSED_STORAGE_KEY) === 'true'; } catch { return false; }
}

export function rememberGuideClosed(storage: () => ViewStorage = browserStorage): void {
  try { storage().setItem(GUIDE_CLOSED_STORAGE_KEY, 'true'); } catch {}
}

function validPreferences(value: unknown): value is ViewPreferences {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const entry = value as Record<string, unknown>;
  return Object.keys(entry).length === 3
    && Object.hasOwn(entry, 'theme') && Object.hasOwn(entry, 'sidebarWidth')
    && Object.hasOwn(entry, 'rosterCollapsed')
    && (entry.theme === 'system' || entry.theme === 'light' || entry.theme === 'dark')
    && typeof entry.sidebarWidth === 'number' && Number.isInteger(entry.sidebarWidth)
    && entry.sidebarWidth >= MIN_SIDEBAR_WIDTH && entry.sidebarWidth <= MAX_SIDEBAR_WIDTH
    && typeof entry.rosterCollapsed === 'boolean';
}

export function readViewPreferences(storage: () => ViewStorage = browserStorage): ViewPreferences {
  try {
    const raw = storage().getItem(STORAGE_KEY);
    if (raw === null) return DEFAULT_PREFERENCES;
    const value: unknown = JSON.parse(raw);
    return validPreferences(value) ? value : DEFAULT_PREFERENCES;
  } catch {
    return DEFAULT_PREFERENCES;
  }
}

export function writeViewPreferences(
  preferences: ViewPreferences, storage: () => ViewStorage = browserStorage,
): void {
  try {
    storage().setItem(STORAGE_KEY, JSON.stringify({
      theme: preferences.theme,
      sidebarWidth: preferences.sidebarWidth,
      rosterCollapsed: preferences.rosterCollapsed,
    }));
  } catch {
    // Browser storage is optional for view preferences.
  }
}

/** The office this browser pins, or `null` for the automatic choice. Anything else stored reads as automatic. */
export function readOfficePin(storage: () => ViewStorage = browserStorage): LayoutId | null {
  try {
    const value = storage().getItem(OFFICE_PIN_STORAGE_KEY);
    return isLayoutId(value) ? value : null;
  } catch {
    return null;
  }
}

export function writeOfficePin(pin: LayoutId | null, storage: () => ViewStorage = browserStorage): void {
  try { storage().setItem(OFFICE_PIN_STORAGE_KEY, pin ?? 'automatic'); } catch {}
}

let officePin: LayoutId | null | undefined;
const officePinListeners = new Set<() => void>();

/** The pin as Settings last set it, read from storage once per page. */
export function getOfficePin(): LayoutId | null {
  if (officePin === undefined) officePin = readOfficePin();
  return officePin;
}

export function setOfficePin(pin: LayoutId | null): void {
  officePin = pin;
  writeOfficePin(pin);
  for (const listener of officePinListeners) listener();
}

export function subscribeToOfficePin(listener: () => void): () => void {
  officePinListeners.add(listener);
  return () => { officePinListeners.delete(listener); };
}

function validActivityFilters(value: unknown): value is ActivityFilters {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return false;
  const entry = value as Record<string, unknown>;
  return Object.keys(entry).length === 3
    && typeof entry.agentId === 'string' && (entry.agentId === '' || AGENT_ID.test(entry.agentId))
    && typeof entry.kind === 'string' && (entry.kind === '' || ACTIVITY_KIND.test(entry.kind))
    && typeof entry.query === 'string' && entry.query.length <= MAX_ACTIVITY_QUERY;
}

export function readActivityFilters(storage: () => ViewStorage = browserStorage): ActivityFilters {
  try {
    const raw = storage().getItem(ACTIVITY_FILTERS_STORAGE_KEY);
    if (raw === null || raw.length > 1024) return DEFAULT_ACTIVITY_FILTERS;
    const value: unknown = JSON.parse(raw);
    return validActivityFilters(value) ? value : DEFAULT_ACTIVITY_FILTERS;
  } catch {
    return DEFAULT_ACTIVITY_FILTERS;
  }
}

export function writeActivityFilters(filters: ActivityFilters, storage: () => ViewStorage = browserStorage): void {
  if (!validActivityFilters(filters)) return;
  try {
    storage().setItem(ACTIVITY_FILTERS_STORAGE_KEY, JSON.stringify({
      agentId: filters.agentId, kind: filters.kind, query: filters.query,
    }));
  } catch {}
}

export function focusAgentId(
  floor: Pick<FloorState, 'phase' | 'stale' | 'epoch' | 'snapshotEpoch' | 'agents'>,
  selectedId: string | null,
): string | null {
  if (floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return null;
  if (selectedId !== null && floor.agents.some((row) => row.id === selectedId)) return selectedId;
  return floor.agents[0]?.id ?? null;
}

export function isFocusShortcut(
  event: Pick<KeyboardEvent, 'code' | 'altKey' | 'shiftKey' | 'ctrlKey' | 'metaKey' | 'repeat'>,
  blocked: boolean,
): boolean {
  return !blocked && !event.repeat && event.code === 'KeyF' && event.altKey && event.shiftKey
    && !event.ctrlKey && !event.metaKey;
}

export function toggleTheme(theme: ThemePreference, osDark: boolean): Exclude<ThemePreference, 'system'> {
  const current = theme === 'system' ? (osDark ? 'dark' : 'light') : theme;
  return current === 'dark' ? 'light' : 'dark';
}

function clampSidebarWidth(width: number): number {
  return Math.max(MIN_SIDEBAR_WIDTH, Math.min(MAX_SIDEBAR_WIDTH, Math.round(width)));
}

export function pointerSidebarWidth(initialWidth: number, startX: number, currentX: number): number {
  return clampSidebarWidth(initialWidth + startX - currentX);
}

export function keyboardSidebarWidth(width: number, key: string): number | null {
  switch (key) {
    case 'ArrowLeft': return clampSidebarWidth(width + SIDEBAR_STEP);
    case 'ArrowRight': return clampSidebarWidth(width - SIDEBAR_STEP);
    case 'Home': return MIN_SIDEBAR_WIDTH;
    case 'End': return MAX_SIDEBAR_WIDTH;
    default: return null;
  }
}

export type AgentOrderView = 'strip' | 'focus';
const AGENT_ID = /^[0-9A-HJKMNP-TV-Z]{26}$/;
const MAX_ORDER = 1024;

function validOrder(value: unknown): value is readonly string[] {
  return Array.isArray(value) && value.length <= MAX_ORDER
    && value.every((id) => typeof id === 'string' && AGENT_ID.test(id))
    && new Set(value).size === value.length;
}

export function readAgentOrder(view: AgentOrderView, storage: () => ViewStorage = browserStorage): readonly string[] {
  return readAgentIds(`dudes.floor.agent-order.${view}`, storage);
}

function readAgentIds(key: string, storage: () => ViewStorage): readonly string[] {
  try {
    const raw = storage().getItem(key);
    if (raw === null || raw.length > MAX_ORDER * 30) return [];
    const value: unknown = JSON.parse(raw);
    return validOrder(value) ? value : [];
  } catch {
    return [];
  }
}

export function writeAgentOrder(
  view: AgentOrderView, ids: readonly string[], storage: () => ViewStorage = browserStorage,
): boolean {
  return writeAgentIds(`dudes.floor.agent-order.${view}`, ids, storage);
}

function writeAgentIds(key: string, ids: readonly string[], storage: () => ViewStorage): boolean {
  try {
    if (!validOrder(ids)) return false;
    storage().setItem(key, JSON.stringify(ids));
    return true;
  } catch {
    return false;
  }
}

export function readHiddenArchiveIds(storage: () => ViewStorage = browserStorage): readonly string[] {
  return readAgentIds('dudes.floor.hidden-archive', storage);
}

export function writeHiddenArchiveIds(ids: readonly string[], storage: () => ViewStorage = browserStorage): boolean {
  return writeAgentIds('dudes.floor.hidden-archive', ids, storage);
}

export function visibleArchivedAgents(
  rows: readonly RetiredAgentRow[], hidden: readonly string[], showHidden: boolean,
): readonly RetiredAgentRow[] {
  return showHidden ? rows : rows.filter((row) => !hidden.includes(row.id));
}

export function agentOrder<T extends { readonly id: string }>(rows: readonly T[], ids: readonly string[]): readonly T[] {
  const remaining = new Map(rows.map((row) => [row.id, row]));
  const ordered: T[] = [];
  for (const id of ids) {
    const row = remaining.get(id);
    if (row !== undefined) {
      ordered.push(row);
      remaining.delete(id);
    }
  }
  return [...ordered, ...remaining.values()];
}

export function moveAgent(ids: readonly string[], agent: string, target: string): readonly string[] {
  const from = ids.indexOf(agent);
  const to = ids.indexOf(target);
  if (from === -1 || to === -1 || from === to) return ids;
  const reordered = [...ids];
  reordered.splice(from, 1);
  reordered.splice(to, 0, agent);
  return reordered;
}

export function reorderKeyTarget(ids: readonly string[], agent: string, key: string): string | null {
  const at = ids.indexOf(agent);
  if (at === -1) return null;
  switch (key) {
    case 'ArrowLeft': case 'ArrowUp': return ids[at - 1] ?? null;
    case 'ArrowRight': case 'ArrowDown': return ids[at + 1] ?? null;
    case 'Home': return at === 0 ? null : ids[0] ?? null;
    case 'End': return at === ids.length - 1 ? null : ids.at(-1) ?? null;
    default: return null;
  }
}

const DETAIL_TAB_STORAGE_KEY = 'dudes.floor.agent-detail-tab';

export function readDetailTab(storage: () => ViewStorage = browserStorage, fallback: DetailTab = 'overview'): DetailTab {
  try {
    const value = storage().getItem(DETAIL_TAB_STORAGE_KEY);
    return value === 'overview' || value === 'output' || value === 'terminal' || value === 'git' || value === 'memory'
      || value === 'traces' || value === 'messages' || value === 'chat' ? value : fallback;
  } catch {
    return fallback;
  }
}

export function writeDetailTab(tab: DetailTab, storage: () => ViewStorage = browserStorage): void {
  try { storage().setItem(DETAIL_TAB_STORAGE_KEY, tab); } catch {}
}
