// SPDX-License-Identifier: MIT OR Apache-2.0

import { cn } from '../../ui/index.ts';
import { PRIORITY_WORDS } from './priority.ts';
import type { Priority } from './priority.ts';

const LOOK: Readonly<Record<Exclude<Priority, 'none'>, { readonly chip: string; readonly dot: string }>> = Object.freeze({
  urgent: { chip: 'bg-danger-soft', dot: 'bg-danger' },
  high: { chip: 'bg-warn-soft', dot: 'bg-warn' },
  medium: { chip: 'bg-info-soft', dot: 'bg-info' },
  low: { chip: 'bg-surface-2', dot: 'bg-subtle' },
});

/** The word for a ticket's priority in a small chip; a ticket with no priority shows nothing. */
export function PriorityMark({ priority, className }: { readonly priority: Priority; readonly className?: string }) {
  if (priority === 'none') return null;
  const look = LOOK[priority];
  return <span data-priority={priority} title={`${PRIORITY_WORDS[priority]} priority`}
    className={cn('ticket-priority inline-flex h-5 shrink-0 items-center gap-1.5 rounded-pill px-2 font-sans text-caption leading-none font-medium whitespace-nowrap text-ink', look.chip, className)}>
    <span aria-hidden="true" className={cn('size-1.5 rounded-pill', look.dot)} />
    {PRIORITY_WORDS[priority]}
  </span>;
}
