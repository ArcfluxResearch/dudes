import { LoaderCircle } from 'lucide-react';
import type { ButtonHTMLAttributes, ReactNode, Ref } from 'react';

import { ICON, RING, cx } from './cx.ts';

export type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'danger';
export type ButtonSize = 'sm' | 'md' | 'lg';

const VARIANT: Readonly<Record<ButtonVariant, string>> = {
  primary: 'border-transparent bg-accent text-accent-fg hover:bg-accent-hover',
  secondary: 'border-line-strong bg-surface text-ink hover:bg-surface-2',
  ghost: 'border-transparent bg-transparent text-muted hover:bg-surface-2 hover:text-ink',
  danger: 'border-transparent bg-danger text-accent-fg hover:bg-danger/90',
};

const SIZE: Readonly<Record<ButtonSize, { readonly text: string; readonly icon: string }>> = {
  sm: { text: 'h-7 px-2', icon: 'size-7' },
  md: { text: 'h-8 px-3', icon: 'size-8' },
  lg: { text: 'h-9 px-4', icon: 'size-9' },
};

export const BUTTON_BASE = cx(
  'ui box-border m-0 inline-flex shrink-0 items-center justify-center gap-1.5 rounded-md border',
  'font-sans text-base font-medium leading-none whitespace-nowrap select-none cursor-pointer no-underline',
  'transition-colors active:translate-y-px waiting:cursor-not-allowed waiting:opacity-50',
  'disabled:cursor-not-allowed disabled:opacity-50', RING,
);

type Native = Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'children' | 'aria-label'>;

export type ButtonProps = Native & {
  readonly variant?: ButtonVariant;
  readonly size?: ButtonSize;
  readonly loading?: boolean;
  readonly ref?: Ref<HTMLButtonElement>;
} & (
  | { readonly children: ReactNode; readonly icon?: ReactNode; readonly label?: string; readonly iconOnly?: never }
  | { readonly iconOnly: ReactNode; readonly label: string; readonly children?: never; readonly icon?: never }
);

/** A button. An icon-only button must carry the accessible label that names it. */
export function Button({ variant = 'secondary', size = 'md', loading = false, icon, iconOnly, label, children, className, type = 'button', ref, ...rest }: ButtonProps) {
  const only = iconOnly !== undefined;
  const state = loading ? { 'aria-busy': true, 'aria-disabled': true, ...rest } : rest;
  return <button ref={ref} type={type}
    className={cx(BUTTON_BASE, VARIANT[variant], only ? SIZE[size].icon : SIZE[size].text, className)} aria-label={label} {...state}>
    {loading ? <LoaderCircle {...ICON} className="animate-spin" /> : only ? iconOnly : icon}
    {only ? null : children}
  </button>;
}
