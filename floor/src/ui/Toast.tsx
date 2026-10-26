// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ComponentPropsWithRef, ReactNode } from 'react';

import { Button } from './Button.tsx';
import { AlertIcon, InfoIcon, SuccessIcon } from './icons.tsx';
import './base.css';
import './Toast.css';

export type ToastTone = 'done' | 'info' | 'problem';

export interface ToastProps extends Omit<ComponentPropsWithRef<'p'>, 'role'> {
  readonly tone?: ToastTone;
  readonly children: ReactNode;
  /** One optional action, drawn before Dismiss. */
  readonly action?: ReactNode;
  readonly onDismiss: () => void;
  readonly dismissClassName?: string;
}

/** A non-modal message. It is a status region, so a screen reader hears it without it taking focus. */
export function Toast({ tone = 'info', children, action, onDismiss, dismissClassName, ...rest }: ToastProps) {
  return (
    <p role="status" {...rest} data-tone={tone} data-ui="toast" data-ui-scope="">
      {tone === 'done' ? <SuccessIcon className="ui-toast-icon" /> : tone === 'problem' ? <AlertIcon className="ui-toast-icon" />
        : <InfoIcon className="ui-toast-icon" />}
      <span className="ui-toast-message">{children}</span>
      <span className="ui-toast-actions">
        {action}
        <Button variant="ghost" size="sm" className={dismissClassName} onClick={onDismiss}>Dismiss</Button>
      </span>
    </p>
  );
}
