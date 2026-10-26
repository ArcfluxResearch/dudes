// SPDX-License-Identifier: MIT OR Apache-2.0

import { useId } from 'react';
import type { ReactNode } from 'react';

import { cx } from './cx.ts';
import './base.css';
import './Switch.css';

export interface SwitchProps {
  readonly label: ReactNode;
  readonly description?: ReactNode;
  readonly checked: boolean;
  readonly onCheckedChange: (next: boolean) => void;
  /** While a change is being saved. The switch stays focusable and ignores presses. */
  readonly busy?: boolean;
  readonly busyTitle?: string;
  readonly disabled?: boolean;
  readonly className?: string;
  /** What the daemon said about this switch after the last press, drawn under the row. */
  readonly children?: ReactNode;
}

export function Switch({ label, description, checked, onCheckedChange, busy = false, busyTitle, disabled = false, className, children }: SwitchProps) {
  const id = useId();
  const held = busy || disabled;
  return (
    <div className={cx(className, 'ui-switch-row')}>
      <div className="ui-switch-text">
        <span className="ui-switch-label" id={`${id}-label`}>{label}</span>
        {description === undefined ? null : <span className="ui-switch-desc" id={`${id}-desc`}>{description}</span>}
      </div>
      <button type="button" role="switch" className="ui-switch" aria-checked={checked} aria-labelledby={`${id}-label`}
        aria-describedby={description === undefined ? undefined : `${id}-desc`} aria-disabled={held || undefined}
        title={busy ? busyTitle : undefined}
        onClick={() => { if (!held) onCheckedChange(!checked); }}>
        <span className="ui-switch-thumb" />
      </button>
      {children === undefined || children === null || children === false ? null : <div className="ui-switch-outcome">{children}</div>}
    </div>
  );
}
