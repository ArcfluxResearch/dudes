// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ReactNode } from 'react';

import { Card, RING, cn } from '../ui/index.ts';

/** The look of a link in running text: the accent colour, underlined, with the one focus ring. */
export const LINK = cn('rounded-sm text-accent underline underline-offset-2 hover:text-accent-hover', RING);

/** Plain text under a heading or beside a control. */
export const HINT = 'm-0 text-base leading-reading text-muted';

/** A group of related settings or facts on one card: a heading, an optional line under it, and the body. */
export function Group({ title, headingId, description, action, children, className }: {
  readonly title: ReactNode;
  readonly headingId: string;
  readonly description?: ReactNode;
  /** A control that belongs to the whole group, at the right of its heading. */
  readonly action?: ReactNode;
  readonly children: ReactNode;
  readonly className?: string;
}) {
  return (
    <Card role="region" aria-labelledby={headingId} className={cn('grid gap-3 p-4', className)}>
      <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-2">
        <div className="grid min-w-0 gap-1">
          <h3 id={headingId} className="m-0 text-title font-semibold text-ink">{title}</h3>
          {description === undefined ? null : <p className={HINT}>{description}</p>}
        </div>
        {action === undefined ? null : <div className="flex flex-wrap items-center gap-2">{action}</div>}
      </div>
      {children}
    </Card>
  );
}

/** A command or an identifier, set apart in the mono face so it can be read and copied exactly. */
export function Code({ children, className }: { readonly children: ReactNode; readonly className?: string }) {
  return <code className={cn('rounded-sm bg-surface-2 px-1.5 py-0.5 font-mono text-small text-ink [overflow-wrap:anywhere]', className)}>{children}</code>;
}

const TONE = { ok: 'bg-ok-soft', warn: 'bg-warn-soft', neutral: 'bg-surface-2', accent: 'bg-accent-soft' } as const;
const DOT = { ok: 'bg-ok', warn: 'bg-warn', neutral: 'bg-subtle', accent: 'bg-accent' } as const;

/** A short word on a tinted chip, with a dot that repeats the tone. The word carries the meaning. */
export function Tag({ tone = 'neutral', children, className }: {
  readonly tone?: keyof typeof TONE;
  readonly children: ReactNode;
  readonly className?: string;
}) {
  return (
    <span data-tone={tone} className={cn('inline-flex h-5 items-center gap-1.5 rounded-pill px-2 text-caption font-medium leading-none whitespace-nowrap text-ink', TONE[tone], className)}>
      <span aria-hidden="true" className={cn('size-1.5 rounded-pill', DOT[tone])} />
      {children}
    </span>
  );
}

/** Label and value pairs, one per row, with a hairline between rows. */
export function Facts({ children, className }: { readonly children: ReactNode; readonly className?: string }) {
  return <dl className={cn('m-0 grid divide-y divide-line', className)}>{children}</dl>;
}

export function Fact({ label, children }: { readonly label: ReactNode; readonly children: ReactNode }) {
  return (
    <div className="grid gap-0.5 py-2 first:pt-0 last:pb-0 min-[641px]:grid-cols-[minmax(0,2fr)_minmax(0,3fr)] min-[641px]:gap-4">
      <dt className="text-base font-medium text-ink">{label}</dt>
      <dd className="m-0 min-w-0 text-base text-muted [overflow-wrap:anywhere]">{children}</dd>
    </div>
  );
}
