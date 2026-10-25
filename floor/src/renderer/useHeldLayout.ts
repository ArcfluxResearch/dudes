// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, useState } from 'react';

import type { NeedRow, NeedState } from './needs.ts';

export interface Placed {
  readonly row: NeedRow;
  /** The state the row is listed under, which stays put while the list is held even if the row's own state changes. */
  readonly group: NeedState;
}

/** The rows in the order and groups `frozen` placed them, with each row's current facts. New agents go last. */
export function pinned(frozen: readonly Placed[], rows: readonly NeedRow[]): readonly Placed[] {
  const current = new Map(rows.map((row) => [row.id, row] as const));
  const kept = frozen.flatMap((place) => {
    const row = current.get(place.row.id);
    return row === undefined ? [] : [{ row, group: place.group }];
  });
  const known = new Set(kept.map((place) => place.row.id));
  return [...kept, ...rows.filter((row) => !known.has(row.id)).map((row) => ({ row, group: row.state }))];
}

/**
 * Rows in the order `rows` gives, except while the pointer or keyboard focus is inside the list element
 * `attach` was given: then the order and groups from just before stand, so a row never moves under a click.
 */
export function useHeldLayout(rows: readonly NeedRow[]): {
  readonly attach: (list: HTMLElement | null) => void;
  readonly placed: readonly Placed[];
} {
  const [list, attach] = useState<HTMLElement | null>(null);
  const [pointer, setPointer] = useState(false);
  const [focus, setFocus] = useState(false);
  const last = useRef<readonly Placed[]>([]);
  useEffect(() => {
    if (list === null) return undefined;
    const enter = () => setPointer(true);
    const leave = () => setPointer(false);
    const focusIn = () => setFocus(true);
    const focusOut = (event: FocusEvent) => {
      if (!(event.relatedTarget instanceof Node && list.contains(event.relatedTarget))) setFocus(false);
    };
    list.addEventListener('pointerenter', enter);
    list.addEventListener('pointerleave', leave);
    list.addEventListener('focusin', focusIn);
    list.addEventListener('focusout', focusOut);
    return () => {
      list.removeEventListener('pointerenter', enter);
      list.removeEventListener('pointerleave', leave);
      list.removeEventListener('focusin', focusIn);
      list.removeEventListener('focusout', focusOut);
    };
  }, [list]);
  const placed = (pointer || focus) && last.current.length > 0
    ? pinned(last.current, rows)
    : rows.map((row) => ({ row, group: row.state }));
  last.current = placed;
  return { attach, placed };
}
