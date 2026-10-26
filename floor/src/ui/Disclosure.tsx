import { ChevronRight } from 'lucide-react';
import type { DetailsHTMLAttributes, ReactNode } from 'react';

import { ICON, RING, cx } from './cx.ts';

/** A restyled `details`: the chevron turns when it opens, and the native keyboard and find-in-page behaviour stay. */
export function Disclosure({ summary, children, className, summaryClassName, ...rest }: {
  readonly summary: ReactNode;
  readonly children: ReactNode;
  readonly summaryClassName?: string;
} & Omit<DetailsHTMLAttributes<HTMLDetailsElement>, 'children'>) {
  return <details {...rest} className={cx('ui group font-sans text-base text-ink', className)}>
    <summary className={cx('ui flex min-h-7 cursor-pointer list-none items-center gap-1.5 rounded-md py-1 font-medium text-muted select-none hover:text-ink [&::-webkit-details-marker]:hidden', RING, summaryClassName)}>
      <ChevronRight {...ICON} className="shrink-0 transition-transform group-open:rotate-90" />
      {summary}
    </summary>
    <div className="pt-2 pl-5">{children}</div>
  </details>;
}
