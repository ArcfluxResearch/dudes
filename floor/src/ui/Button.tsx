import { LoaderCircle } from 'lucide-react';
import type { ButtonHTMLAttributes, MouseEvent, ReactNode, Ref } from 'react';

import { cn } from './cn.ts';
import { ICON, RING } from './look.ts';

export type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'danger';
export type ButtonSize = 'sm' | 'md';

const VARIANT: Readonly<Record<ButtonVariant, string>> = {
  primary: 'border-transparent bg-accent text-accent-fg hover:bg-accent-hover active:bg-accent-hover',
  secondary: 'border-line-strong bg-surface text-ink hover:bg-surface-2 active:bg-line',
  ghost: 'border-transparent bg-transparent text-muted hover:bg-surface-2 hover:text-ink active:bg-line',
  danger: 'border-transparent bg-danger text-accent-fg hover:bg-danger/90 active:bg-danger/80',
};

const SIZE: Readonly<Record<ButtonSize, { readonly text: string; readonly square: string }>> = {
  sm: { text: 'h-7 px-2 max-[801px]:min-h-11', square: 'size-7 max-[801px]:min-h-11 max-[801px]:min-w-11' },
  md: { text: 'h-8 px-3 max-[801px]:min-h-11', square: 'size-8 max-[801px]:min-h-11 max-[801px]:min-w-11' },
};

const BASE = cn(
  'box-border m-0 inline-flex shrink-0 items-center justify-center gap-1.5 rounded-md border',
  'cursor-pointer font-sans text-base font-medium leading-none whitespace-nowrap no-underline select-none',
  'transition-[color,background-color,border-color,opacity] active:translate-y-px',
  'disabled:cursor-not-allowed disabled:opacity-50 waiting:cursor-not-allowed waiting:opacity-50',
  RING,
);

/** The class string every button draws from, for a control that cannot be a Button but must look like one. */
export function buttonClass(variant: ButtonVariant, size: ButtonSize, square: boolean, className?: string): string {
  return cn(BASE, VARIANT[variant], square ? SIZE[size].square : SIZE[size].text, className);
}

type Native = Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'children'>;

export type ButtonProps = Native & {
  readonly variant?: ButtonVariant;
  readonly size?: ButtonSize;
  /** An icon before the text. */
  readonly icon?: ReactNode;
  /** Shows a spinner, marks the button busy and ignores presses. The button keeps the keyboard focus. */
  readonly loading?: boolean;
  readonly children: ReactNode;
  readonly ref?: Ref<HTMLButtonElement>;
};

/** A press handler that does nothing while the button is waiting on an answer, so it can stay focusable instead of disabled. */
export function whileIdle(waiting: boolean, onClick: ButtonProps['onClick']) {
  return (event: MouseEvent<HTMLButtonElement>) => {
    if (waiting) event.preventDefault();
    else onClick?.(event);
  };
}

export function isWaiting(loading: boolean, ariaDisabled: ButtonProps['aria-disabled']): boolean {
  return loading || ariaDisabled === true || ariaDisabled === 'true';
}

export function Button({ variant = 'secondary', size = 'md', icon, loading = false, children, className, type = 'button', onClick, ref, ...rest }: ButtonProps) {
  const waiting = isWaiting(loading, rest['aria-disabled']);
  return (
    <button ref={ref} type={type} data-ui="button" data-variant={variant} data-size={size} data-loading={loading || undefined}
      {...rest} aria-busy={loading || undefined} aria-disabled={waiting || undefined} onClick={whileIdle(waiting, onClick)}
      className={buttonClass(variant, size, false, className)}>
      {loading ? <LoaderCircle {...ICON} className="animate-spin" /> : icon}
      {children}
    </button>
  );
}
