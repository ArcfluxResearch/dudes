// SPDX-License-Identifier: MIT OR Apache-2.0

import type { RefCallback } from 'react';

export const AGENT_DETAIL_HEADING_ID = 'agent-detail-title';

export const handFocusOnLeave: RefCallback<HTMLElement> = (node) => {
  if (node === null) return undefined;
  // React runs this before removing the node, while it can still contain the focused descendant.
  return () => {
    const heading = node.dataset.focusOnLeave;
    if (heading === undefined || !node.contains(node.ownerDocument.activeElement)) return;
    node.ownerDocument.getElementById(heading)?.focus();
  };
};

/**
 * As handFocusOnLeave, once the leaving element is gone, for a target that
 * shows only then, such as the next Done notice beside a terminal.
 */
export const handFocusAfterLeave: RefCallback<HTMLElement> = (node) => {
  if (node === null) return undefined;
  return () => {
    const next = node.dataset.focusOnLeave;
    if (next === undefined || !node.contains(node.ownerDocument.activeElement)) return;
    queueMicrotask(() => node.ownerDocument.getElementById(next)?.focus());
  };
};

interface PanelKey {
  readonly key: string;
  readonly defaultPrevented: boolean;
  readonly nativeEvent: { readonly isComposing: boolean };
}

/** Escape closes a panel unless a field inside it already used the key or an input method is composing. */
export function closesPanel(event: PanelKey): boolean {
  return event.key === 'Escape' && !event.defaultPrevented && !event.nativeEvent.isComposing;
}

/** Escape anywhere on the stage closes the open drawer, but not from the terminal, whose keys go to the agent. */
export function closesDrawer(event: PanelKey & { readonly target: unknown }, open: boolean): boolean {
  const target = event.target as { closest?: (selector: string) => unknown } | null;
  const inTerminal = typeof target?.closest === 'function' && target.closest('.terminal-panel') !== null;
  return open && !inTerminal && closesPanel(event);
}

export function restoreFocusAfterDialog(
  opener: Pick<HTMLElement, 'isConnected' | 'focus'> & { readonly disabled?: boolean; readonly ownerDocument?: Document } | null,
  fallback: Pick<HTMLElement, 'focus'> | null,
  schedule: (callback: () => void) => void = (callback) => { requestAnimationFrame(callback); },
): void {
  schedule(() => {
    if (opener?.isConnected && opener.disabled !== true) {
      opener.focus();
      if (opener.ownerDocument === undefined || Object.is(opener.ownerDocument.activeElement, opener)) return;
    }
    fallback?.focus();
  });
}

/**
 * Where a speech mark in the office sends the keyboard: the drawer's answer
 * box, or the waiting question itself while answering is off and there is no
 * box to type in.
 */
export function focusAnswer(drawer: Pick<HTMLElement, 'querySelector'> | null): void {
  (drawer?.querySelector<HTMLElement>('.agent-detail-answer textarea')
    ?? drawer?.querySelector<HTMLElement>('.agent-detail-question'))?.focus();
}
