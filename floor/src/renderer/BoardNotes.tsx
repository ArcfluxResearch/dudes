// SPDX-License-Identifier: MIT OR Apache-2.0

import type { Ref } from 'react';

import { cn, RING } from '../ui/index.ts';

/** The native button over the office's goal board, which opens the Board view. The office places it over the board each frame. */
export function BoardNotes({ count, hidden, goalsHidden = 0, buttonRef, onOpenTasks }: {
  readonly count: number;
  /** Board cards past the board's slots, which the board counts but does not draw. */
  readonly hidden: number;
  /** Goals past the board's goal notes, counted the same way. */
  readonly goalsHidden?: number;
  readonly buttonRef?: Ref<HTMLButtonElement>;
  readonly onOpenTasks: () => void;
}) {
  const cards = `${count} ${count === 1 ? 'card' : 'cards'}`;
  const goals = goalsHidden === 1 ? 'goal' : 'goals';
  // The name starts with the count the button shows, so a voice user can say what they see; a screen reader hears what it counts.
  const name = `${cards}${hidden === 0 ? '' : `, ${hidden} not on the board`}`
    + `${goalsHidden === 0 ? '' : `, ${goalsHidden} ${goals} not on the board`}, open Board`;
  return <button type="button" className={cn('board-select absolute box-border cursor-pointer border-0 bg-transparent p-0 outline-0 hover:bg-accent/10 hover:outline-2',
    'hover:outline-solid hover:outline-offset-2 hover:outline-accent', RING, 'focus-visible:outline-ink focus-visible:shadow-[0_0_0_7px_var(--ui-surface)]')} ref={buttonRef} aria-label={name}
    aria-controls="control-panel" title={`${cards} on Board`} onClick={onOpenTasks}>
    <span className="board-note-count absolute top-1/2 left-1/2 max-w-[90%] -translate-x-1/2 -translate-y-1/2 overflow-hidden rounded-sm bg-surface px-1 py-px font-mono text-[9px] whitespace-nowrap text-ink">{cards}{hidden === 0 ? '' : ` · +${hidden}`}{goalsHidden === 0 ? '' : ` · +${goalsHidden} ${goals}`}</span>
  </button>;
}
