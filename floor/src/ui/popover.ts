// SPDX-License-Identifier: MIT OR Apache-2.0

import { useCallback, useEffect, useLayoutEffect, useRef } from 'react';
import type { RefObject } from 'react';

const GAP = 4;

/**
 * Keeps a `popover="manual"` panel open beside its trigger. The panel stays a DOM child of the component, so inside a modal
 * `dialog` it is still part of the dialog and takes its clicks, and the top layer draws it above the dialog and above any
 * scrolling ancestor. Position is set through the style object model, which the page's content policy allows.
 */
export function useAnchoredPopover(
  open: boolean,
  trigger: RefObject<HTMLElement | null>,
  panel: RefObject<HTMLElement | null>,
  close: (returnFocus: boolean) => void,
  align: 'start' | 'end' = 'start',
  matchWidth = false,
): void {
  const place = useCallback(() => {
    const anchor = trigger.current;
    const box = panel.current;
    if (anchor === null || box === null) return;
    const at = anchor.getBoundingClientRect();
    const width = Math.max(at.width, box.offsetWidth);
    if (matchWidth) box.style.minWidth = `${at.width}px`;
    const left = align === 'end' ? at.right - width : at.left;
    box.style.left = `${Math.max(8, Math.min(left, window.innerWidth - width - 8))}px`;
    const room = window.innerHeight - at.bottom - GAP - 8;
    const flip = box.offsetHeight > room && at.top > room;
    box.style.top = flip ? `${Math.max(8, at.top - GAP - box.offsetHeight)}px` : `${at.bottom + GAP}px`;
    box.style.maxHeight = `${Math.max(120, flip ? at.top - GAP - 8 : room)}px`;
  }, [trigger, panel, align, matchWidth]);

  useLayoutEffect(() => {
    const box = panel.current;
    if (box === null) return undefined;
    if (!open) {
      try { if (box.matches(':popover-open')) box.hidePopover(); } catch { /* already hidden */ }
      return undefined;
    }
    try { box.showPopover(); } catch { /* an unsupported browser shows it through the open attribute rule */ }
    place();
    return () => {
      try { if (box.matches(':popover-open')) box.hidePopover(); } catch { /* already hidden */ }
    };
  }, [open, place, panel]);

  const latest = useRef(close);
  latest.current = close;
  useEffect(() => {
    if (!open) return undefined;
    const outside = (event: PointerEvent) => {
      const target = event.target as Node | null;
      if (target !== null && (trigger.current?.contains(target) || panel.current?.contains(target))) return;
      latest.current(false);
    };
    const move = () => place();
    document.addEventListener('pointerdown', outside, true);
    window.addEventListener('resize', move);
    window.addEventListener('scroll', move, true);
    return () => {
      document.removeEventListener('pointerdown', outside, true);
      window.removeEventListener('resize', move);
      window.removeEventListener('scroll', move, true);
    };
  }, [open, place, trigger, panel]);
}
