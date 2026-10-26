import type { HTMLAttributes, ReactNode, Ref } from 'react';

import { cn } from './cn.ts';

/** A surface with an edge and a soft shadow. */
export function Card({ className, ref, ...rest }: HTMLAttributes<HTMLDivElement> & { readonly ref?: Ref<HTMLDivElement> }) {
  return <div ref={ref} data-ui="card" {...rest}
    className={cn('box-border rounded-lg border border-line bg-surface p-4 font-sans text-ink shadow-card', className)} />;
}

/** A named region: a heading, its actions at the right, and the body under them. */
export function Panel({ title, headingId, actions, children, className, ...rest }: {
  readonly title: ReactNode;
  /** The id of the heading, for `aria-labelledby` from outside. */
  readonly headingId?: string;
  readonly actions?: ReactNode;
  readonly children: ReactNode;
} & Omit<HTMLAttributes<HTMLElement>, 'title' | 'children'>) {
  return (
    <section data-ui="panel" aria-labelledby={headingId} {...rest}
      className={cn('box-border flex flex-col rounded-lg border border-line bg-surface font-sans text-ink shadow-card', className)}>
      <header className="flex min-h-11 items-center justify-between gap-3 border-b border-line px-4 py-2">
        <h2 id={headingId} className="m-0 text-title font-semibold">{title}</h2>
        {actions === undefined ? null : <div className="flex items-center gap-2">{actions}</div>}
      </header>
      <div className="p-4">{children}</div>
    </section>
  );
}
