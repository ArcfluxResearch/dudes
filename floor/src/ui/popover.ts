import { useEffect, useLayoutEffect } from 'react';
import type { RefObject } from 'react';

const GAP = 4;
const EDGE = 8;

/** Puts a shown popover under its anchor, or over it when the room below is short, and keeps it inside the viewport. */
export function placePopover(anchor: HTMLElement, pop: HTMLElement, matchWidth: boolean, alignEnd: boolean): void {
  const rect = anchor.getBoundingClientRect();
  const view = pop.ownerDocument.documentElement;
  const below = view.clientHeight - rect.bottom - GAP - EDGE;
  const above = rect.top - GAP - EDGE;
  pop.style.maxHeight = '';
  pop.style.minWidth = `${Math.round(rect.width)}px`;
  pop.style.width = matchWidth ? `${Math.round(rect.width)}px` : '';
  const wanted = pop.scrollHeight;
  const flip = wanted > below && above > below;
  const room = Math.max(96, flip ? above : below);
  pop.style.maxHeight = `${Math.floor(room)}px`;
  const height = Math.min(wanted, room);
  const top = flip ? rect.top - GAP - height : rect.bottom + GAP;
  const width = pop.offsetWidth;
  const startLeft = alignEnd ? rect.right - width : rect.left;
  const left = Math.max(EDGE, Math.min(startLeft, view.clientWidth - width - EDGE));
  pop.style.top = `${Math.round(top)}px`;
  pop.style.left = `${Math.round(left)}px`;
}

/**
 * Shows `pop` in the top layer while `open`, so it sits above a modal dialog that holds it and is never clipped by
 * the dialog's scrolling. The popover stays a child of the component, so the dialog's inert rule leaves it alone.
 */
export function useTopLayerPopover(
  open: boolean,
  anchor: RefObject<HTMLElement | null>,
  pop: RefObject<HTMLElement | null>,
  onOutside: () => void,
  matchWidth: boolean,
  alignEnd = false,
): void {
  useLayoutEffect(() => {
    const node = pop.current;
    const from = anchor.current;
    if (!open || node === null || from === null) return undefined;
    if (!node.matches(':popover-open')) node.showPopover();
    placePopover(from, node, matchWidth, alignEnd);
    const replace = () => placePopover(from, node, matchWidth, alignEnd);
    window.addEventListener('resize', replace);
    window.addEventListener('scroll', replace, true);
    return () => {
      window.removeEventListener('resize', replace);
      window.removeEventListener('scroll', replace, true);
      if (node.isConnected && node.matches(':popover-open')) node.hidePopover();
    };
  }, [open, anchor, pop, matchWidth, alignEnd]);

  useEffect(() => {
    if (!open) return undefined;
    const away = (event: PointerEvent) => {
      const target = event.target as Node | null;
      if (target === null || anchor.current?.contains(target) || pop.current?.contains(target)) return;
      onOutside();
    };
    document.addEventListener('pointerdown', away, true);
    return () => document.removeEventListener('pointerdown', away, true);
  }, [open, anchor, pop, onOutside]);
}
