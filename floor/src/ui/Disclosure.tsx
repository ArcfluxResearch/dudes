import { ChevronRight } from 'lucide-react';
import type { DetailsHTMLAttributes, ReactNode, Ref } from 'react';

import { cn } from './cn.ts';
import { ICON, RING } from './look.ts';

/** A `details` with a Lucide chevron that turns when it opens, in place of the browser's triangle. */
export function Disclosure({ summary, children, className, summaryClassName, ref, ...rest }: {
  readonly summary: ReactNode;
  readonly children: ReactNode;
  readonly summaryClassName?: string;
  readonly ref?: Ref<HTMLDetailsElement>;
} & Omit<DetailsHTMLAttributes<HTMLDetailsElement>, 'children'>) {
  return (
    <details ref={ref} data-ui="disclosure" {...rest} className={cn('group box-border font-sans text-ink', className)}>
      <summary data-part="summary"
        className={cn('flex cursor-pointer list-none items-center gap-1.5 rounded-md py-1 text-base font-medium marker:hidden max-[801px]:min-h-11',
          'select-none hover:text-ink [&::-webkit-details-marker]:hidden', RING, summaryClassName)}>
        <ChevronRight {...ICON} aria-hidden="true" className="shrink-0 text-muted transition-transform group-open:rotate-90" />
        {summary}
      </summary>
      <div className="pt-1 pl-6 text-base">{children}</div>
    </details>
  );
}
