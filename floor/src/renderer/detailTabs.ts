// SPDX-License-Identifier: MIT OR Apache-2.0

export type DetailTab = 'overview' | 'output' | 'terminal' | 'git' | 'memory' | 'messages' | 'chat';

export function detailTabTarget(tabs: readonly DetailTab[], current: DetailTab, key: string): DetailTab | null {
  const at = tabs.indexOf(current);
  if (at === -1) return null;
  switch (key) {
    case 'ArrowLeft': return tabs[(at + tabs.length - 1) % tabs.length] ?? null;
    case 'ArrowRight': return tabs[(at + 1) % tabs.length] ?? null;
    case 'Home': return tabs[0] ?? null;
    case 'End': return tabs.at(-1) ?? null;
    default: return null;
  }
}

/** The scroll of a tab row `rowWidth` wide, nearest `scrollLeft`, that shows the whole tab at `left`. */
export function revealedTabScroll(scrollLeft: number, left: number, width: number, rowWidth: number): number {
  return Math.min(Math.max(scrollLeft, left + width - rowWidth), left);
}

export function detailTabSelection(tabs: readonly DetailTab[], preferred: DetailTab, focused: DetailTab | null): DetailTab {
  if (focused !== null && tabs.includes(focused)) return focused;
  return tabs.includes(preferred) ? preferred : 'overview';
}
