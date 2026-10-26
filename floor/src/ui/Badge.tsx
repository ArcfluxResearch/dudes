import type { HTMLAttributes } from 'react';

import { cx } from './cx.ts';

export type BadgeState = 'needs-you' | 'ready' | 'working' | 'waiting' | 'done' | 'idle';

export const BADGE_WORDS: Readonly<Record<BadgeState, string>> = {
  'needs-you': 'Needs you',
  ready: 'Ready for you',
  working: 'Working',
  waiting: 'Waiting',
  done: 'Done',
  idle: 'Idle',
};

const TONE: Readonly<Record<BadgeState, { readonly chip: string; readonly dot: string }>> = {
  'needs-you': { chip: 'bg-warn-soft text-warn', dot: 'bg-warn' },
  ready: { chip: 'bg-accent-soft text-accent', dot: 'bg-accent' },
  working: { chip: 'bg-info-soft text-info', dot: 'bg-info' },
  waiting: { chip: 'bg-warn-soft text-warn', dot: 'border border-warn' },
  done: { chip: 'bg-ok-soft text-ok', dot: 'bg-ok' },
  idle: { chip: 'bg-surface-2 text-muted', dot: 'bg-subtle' },
};

/** One of the six state words. The word, not the colour, says the state. */
export function Badge({ state, className, children, ...rest }: { readonly state: BadgeState } & Omit<HTMLAttributes<HTMLSpanElement>, 'children'> & { readonly children?: string }) {
  return <span {...rest} data-state={state}
    className={cx('ui inline-flex h-5 shrink-0 items-center gap-1.5 rounded-sm px-2 font-sans text-small font-medium leading-none whitespace-nowrap', TONE[state].chip, className)}>
    <span aria-hidden="true" className={cx('size-1.5 rounded-pill', TONE[state].dot)} />
    {children ?? BADGE_WORDS[state]}
  </span>;
}
