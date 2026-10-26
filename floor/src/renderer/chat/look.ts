import { cn } from '../../ui/index.ts';

/** The agent's side of the conversation: a bordered card that squares the corner nearest its speaker. */
export const ASSISTANT_BUBBLE = cn('mr-6 min-w-0 rounded-lg rounded-bl-sm border border-line bg-surface px-3 py-2 shadow-card');

/** The person's side: a tinted bubble that squares its own lower corner. */
export const USER_BUBBLE = cn('ml-6 min-w-0 rounded-lg rounded-br-sm bg-accent-soft px-3 py-2');

export const SPEAKER = 'm-0 mb-1 text-small font-semibold text-muted';

/** A hidden character drawn as a visible marker, set from the container that holds the text. */
export const MARKER = cn(
  '[&_.memory-marker]:rounded-sm [&_.memory-marker]:border [&_.memory-marker]:border-dashed [&_.memory-marker]:border-line-strong',
  '[&_.memory-marker]:px-0.5 [&_.memory-marker]:text-muted',
);

/** A line of guidance under a heading or beside a control. */
export const NOTE = 'm-0 text-small leading-normal text-muted';

/** A section heading inside a panel. */
export const HEADING = 'm-0 text-base font-semibold text-ink';
