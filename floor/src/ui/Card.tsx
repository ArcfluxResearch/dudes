import type { HTMLAttributes, ReactNode } from 'react';

import { cx } from './cx.ts';

export function Card({ className, ...rest }: HTMLAttributes<HTMLDivElement>) {
  return <div {...rest} className={cx('ui box-border rounded-lg border border-line bg-surface text-ink shadow-card', className)} />;
}

/** A titled group inside a page: a heading line with optional actions, then the body. */
export function Panel({ title, headingId, actions, children, className, ...rest }: {
  readonly title: ReactNode;
  readonly headingId: string;
  readonly actions?: ReactNode;
} & Omit<HTMLAttributes<HTMLElement>, 'title'>) {
  return <section {...rest} aria-labelledby={headingId} className={cx('ui box-border rounded-lg border border-line bg-surface text-ink shadow-card', className)}>
    <header className="flex min-h-12 items-center justify-between gap-3 border-b border-line px-4 py-2">
      <h3 id={headingId} className="ui m-0 font-sans text-title font-semibold text-ink">{title}</h3>
      {actions}
    </header>
    <div className="p-4">{children}</div>
  </section>;
}
