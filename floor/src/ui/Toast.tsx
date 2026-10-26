import { CircleAlert, CircleCheck, Info, TriangleAlert, X } from 'lucide-react';
import type { ReactNode } from 'react';

import { Button } from './Button.tsx';
import { IconButton } from './IconButton.tsx';
import { cn } from './cn.ts';
import { ICON } from './look.ts';

export type ToastTone = 'info' | 'ok' | 'warn' | 'danger';

const INK: Readonly<Record<ToastTone, string>> = { info: 'text-info', ok: 'text-ok', warn: 'text-warn', danger: 'text-danger' };

function ToneIcon({ tone }: { readonly tone: ToastTone }) {
  const props = { ...ICON, 'aria-hidden': true, className: cn('mt-0.5 shrink-0', INK[tone]) } as const;
  return tone === 'ok' ? <CircleCheck {...props} /> : tone === 'warn' ? <TriangleAlert {...props} />
    : tone === 'danger' ? <CircleAlert {...props} /> : <Info {...props} />;
}

/** A short notice that does not take focus: a `status` region with an icon, a message, an optional action and Dismiss. */
export function Toast({ tone = 'info', action, onDismiss, children, className }: {
  readonly tone?: ToastTone;
  readonly action?: { readonly label: string; readonly onAction: () => void };
  readonly onDismiss: () => void;
  readonly children: ReactNode;
  readonly className?: string;
}) {
  return (
    <div role="status" data-ui="toast" data-tone={tone}
      className={cn('box-border flex w-[min(420px,calc(100vw-32px))] items-start gap-3 rounded-lg border border-line bg-surface p-3 font-sans text-ink shadow-pop', className)}>
      <ToneIcon tone={tone} />
      <p className="m-0 min-w-0 flex-1 text-base leading-normal">{children}</p>
      {action === undefined ? null : <Button variant="ghost" size="sm" onClick={action.onAction}>{action.label}</Button>}
      <IconButton label="Dismiss" icon={<X {...ICON} />} size="sm" onClick={onDismiss} />
    </div>
  );
}
