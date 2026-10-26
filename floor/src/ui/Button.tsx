// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ComponentPropsWithRef, ReactNode } from 'react';

import { SpinnerIcon } from './icons.tsx';
import './base.css';
import './Button.css';

export type ButtonVariant = 'primary' | 'secondary' | 'ghost' | 'danger';
export type ControlSize = 'sm' | 'md' | 'lg';

export interface ButtonProps extends ComponentPropsWithRef<'button'> {
  readonly variant?: ButtonVariant;
  readonly size?: ControlSize;
  /** A leading icon, drawn before the label. */
  readonly icon?: ReactNode;
  /** Shows a spinner and marks the button busy. The button stays focusable, so a keyboard user keeps their place. */
  readonly loading?: boolean;
}

export function Button({ variant = 'secondary', size = 'md', icon, loading = false, className, children, type = 'button', ...rest }: ButtonProps) {
  return (
    <button type={type} className={className} {...rest} aria-busy={rest['aria-busy'] ?? (loading || undefined)}
      data-ui="button" data-variant={variant} data-size={size}>
      {loading ? <SpinnerIcon className="ui-spin" /> : icon}
      {children}
    </button>
  );
}

export interface IconButtonProps extends Omit<ButtonProps, 'children' | 'icon' | 'aria-label'> {
  /** Required: an icon alone says nothing to a screen reader. */
  readonly label: string;
  readonly icon: ReactNode;
}

export function IconButton({ label, icon, className, ...rest }: IconButtonProps) {
  return <Button className={className} icon={icon} aria-label={label} title={rest.title ?? label} {...rest} data-icon-only="" />;
}
