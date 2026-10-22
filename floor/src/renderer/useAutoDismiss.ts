// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef } from 'react';
import type { RefObject } from 'react';

/** How long a success or ready notice stays before it goes by itself. */
export const NOTICE_LINGER_MS = 8_000;

/**
 * Call `dismiss` once `ms` after `key` appears. A notice the pointer is over, or that holds focus, is
 * given another `ms` instead, so it never goes while someone is reading or about to press it.
 */
export function useAutoDismiss(
  ref: RefObject<HTMLElement | null>, key: string | null, dismiss: () => void, ms = NOTICE_LINGER_MS,
): void {
  const latest = useRef(dismiss);
  latest.current = dismiss;
  useEffect(() => {
    if (key === null) return undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const arm = () => {
      timer = setTimeout(() => {
        const node = ref.current;
        if (node !== null && (node.matches(':hover') || node.contains(node.ownerDocument.activeElement))) arm();
        else latest.current();
      }, ms);
    };
    arm();
    return () => clearTimeout(timer);
  }, [key, ms, ref]);
}
