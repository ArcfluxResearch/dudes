// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The board's layout, grouping, filters and search, shared by the board, the list and the link that
// opens the board on one project. The layout, grouping and filters are remembered in this browser.

import { readBoardViewText, writeBoardViewText } from '../view.ts';
import { DEFAULT_VIEW, parseRemembered, serializeRemembered } from './browse.ts';
import type { BrowseView } from './browse.ts';

let current: BrowseView | undefined;
const listeners = new Set<() => void>();

export function getBrowse(): BrowseView {
  current ??= parseRemembered(readBoardViewText());
  return current;
}

export function setBrowse(patch: Partial<BrowseView>): void {
  const before = serializeRemembered(getBrowse());
  const next: BrowseView = Object.freeze({ ...getBrowse(), ...patch });
  const after = serializeRemembered(next);
  if (after !== before) writeBoardViewText(after);
  current = next;
  for (const listener of listeners) listener();
}

export function subscribeToBrowse(listener: () => void): () => void {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
}

/** For a test: forget what this page read, so the next read starts from storage again. */
export function resetBrowse(): void {
  current = undefined;
  for (const listener of listeners) listener();
}

/** Opens the board on one project: that filter alone, with the layout and grouping left as they are. */
export function showProject(project: string): void {
  setBrowse({ project, agent: DEFAULT_VIEW.agent, status: DEFAULT_VIEW.status, search: DEFAULT_VIEW.search });
}
