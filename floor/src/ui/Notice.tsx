import { CircleAlert, CircleCheck, Info, TriangleAlert } from 'lucide-react';
import type { HTMLAttributes, ReactNode } from 'react';

import { cn } from './cn.ts';
import { ICON } from './look.ts';

export type NoticeTone = 'info' | 'ok' | 'warn' | 'danger';

const LOOK: Readonly<Record<NoticeTone, { readonly box: string; readonly icon: string }>> = {
  info: { box: 'border-info/30 bg-info-soft', icon: 'text-info' },
  ok: { box: 'border-ok/30 bg-ok-soft', icon: 'text-ok' },
  warn: { box: 'border-warn/30 bg-warn-soft', icon: 'text-warn' },
  danger: { box: 'border-danger/30 bg-danger-soft', icon: 'text-danger' },
};

function ToneIcon({ tone }: { readonly tone: NoticeTone }) {
  const props = { ...ICON, 'aria-hidden': true, className: cn('mt-0.5 shrink-0', LOOK[tone].icon) } as const;
  return tone === 'ok' ? <CircleCheck {...props} /> : tone === 'warn' ? <TriangleAlert {...props} />
    : tone === 'danger' ? <CircleAlert {...props} /> : <Info {...props} />;
}

/**
 * A message that sits in the page where it applies: a tinted box with an icon, for an error, a warning, a result or a
 * plain fact. The caller gives the role (`alert` for an error, `status` for a result), and a Notice never takes focus.
 */
export function Notice({ tone = 'info', children, className, ...rest }: {
  readonly tone?: NoticeTone;
  readonly children: ReactNode;
} & Omit<HTMLAttributes<HTMLDivElement>, 'children'>) {
  return (
    <div data-ui="notice" data-tone={tone} {...rest}
      className={cn('box-border flex items-start gap-2 rounded-md border p-3 font-sans text-base leading-normal text-ink', LOOK[tone].box, className)}>
      <ToneIcon tone={tone} />
      <div className="flex min-w-0 flex-1 flex-col gap-2 wrap-anywhere">{children}</div>
    </div>
  );
}
