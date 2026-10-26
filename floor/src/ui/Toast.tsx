import { CircleAlert, CircleCheck, Info, TriangleAlert } from 'lucide-react';
import type { HTMLAttributes, ReactNode, Ref } from 'react';

import { Button } from './Button.tsx';
import { ICON, cx } from './cx.ts';

export type ToastTone = 'success' | 'info' | 'warn' | 'danger';

const LOOK: Readonly<Record<ToastTone, { readonly icon: ReactNode; readonly tone: string }>> = {
  success: { icon: <CircleCheck {...ICON} />, tone: 'text-ok' },
  info: { icon: <Info {...ICON} />, tone: 'text-info' },
  warn: { icon: <TriangleAlert {...ICON} />, tone: 'text-warn' },
  danger: { icon: <CircleAlert {...ICON} />, tone: 'text-danger' },
};

/** A non-modal notice: an icon, a message, an optional action, and Dismiss. */
export function Toast({ tone = 'info', action, onDismiss, dismissLabel = 'Dismiss', dismissClassName, children, className, ref, ...rest }: {
  readonly tone?: ToastTone;
  readonly action?: ReactNode;
  readonly onDismiss: () => void;
  readonly dismissLabel?: string;
  readonly dismissClassName?: string;
  readonly children: ReactNode;
  readonly ref?: Ref<HTMLDivElement>;
} & Omit<HTMLAttributes<HTMLDivElement>, 'children'>) {
  return <div ref={ref} role="status" {...rest}
    className={cx('ui box-border flex w-full items-start gap-3 rounded-lg border border-line bg-surface p-3 font-sans text-base text-ink shadow-pop', className)}>
    <span className={cx('mt-0.5 shrink-0', LOOK[tone].tone)}>{LOOK[tone].icon}</span>
    <p className="m-0 min-w-0 flex-1 text-base text-ink [overflow-wrap:anywhere]">{children}</p>
    <div className="flex shrink-0 items-center gap-1">
      {action}
      <Button variant="ghost" size="sm" className={dismissClassName} onClick={onDismiss}>{dismissLabel}</Button>
    </div>
  </div>;
}
