import type { ReactNode } from 'react';

import { cn } from './cn.ts';

export type BadgeState = 'needs-you' | 'ready' | 'working' | 'waiting' | 'done' | 'idle';

const STATE: Readonly<Record<BadgeState, { readonly word: string; readonly chip: string; readonly dot: string }>> = {
  'needs-you': { word: 'Needs you', chip: 'bg-warn-soft', dot: 'bg-warn' },
  ready: { word: 'Ready for you', chip: 'bg-accent-soft', dot: 'bg-accent' },
  working: { word: 'Working', chip: 'bg-info-soft', dot: 'bg-info' },
  waiting: { word: 'Waiting', chip: 'bg-surface-2', dot: 'bg-muted' },
  done: { word: 'Done', chip: 'bg-ok-soft', dot: 'bg-ok' },
  idle: { word: 'Idle', chip: 'bg-surface-2', dot: 'bg-subtle' },
};

/** The six words an agent's state can be. The word says the state; the dot's colour only repeats it. */
export function Badge({ state, children, className }: { readonly state: BadgeState; readonly children?: ReactNode; readonly className?: string }) {
  const look = STATE[state];
  return (
    <span data-ui="badge" data-state={state}
      className={cn('inline-flex h-5 items-center gap-1.5 rounded-pill px-2 font-sans text-caption font-medium leading-none whitespace-nowrap text-ink', look.chip, className)}>
      <span aria-hidden="true" className={cn('size-1.5 rounded-pill', look.dot)} />
      {children ?? look.word}
    </span>
  );
}
