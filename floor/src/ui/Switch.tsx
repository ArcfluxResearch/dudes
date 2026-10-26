import { useId } from 'react';
import type { ButtonHTMLAttributes, ReactNode, Ref } from 'react';

import { RING, cx } from './cx.ts';

/**
 * A labelled on or off control. The state is the caller's: the switch asks for the other value and shows
 * whatever it is handed back, so a setting the daemon keeps never shows a value the daemon does not hold.
 */
export function Switch({ checked, onCheckedChange, label, description, className, ref, ...rest }: {
  readonly checked: boolean;
  readonly onCheckedChange: (next: boolean) => void;
  readonly label: ReactNode;
  readonly description?: ReactNode;
  readonly ref?: Ref<HTMLButtonElement>;
} & Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'children' | 'role'>) {
  const id = useId();
  const described = description === undefined ? undefined : `${id}-description`;
  return <div className={cx('ui flex items-start justify-between gap-4 font-sans', className)}>
    <div className="grid min-w-0 gap-0.5">
      <span id={`${id}-label`} className="text-base font-medium text-ink">{label}</span>
      {description === undefined ? null : <span id={described} className="text-small text-muted">{description}</span>}
    </div>
    <button ref={ref} type="button" role="switch" aria-checked={checked} aria-labelledby={`${id}-label`} aria-describedby={described}
      {...rest} onClick={(event) => { rest.onClick?.(event); if (!event.defaultPrevented) onCheckedChange(!checked); }}
      className={cx('ui relative mt-0.5 inline-flex h-5 w-9 shrink-0 cursor-pointer items-center rounded-pill border-0 p-0 transition-colors waiting:cursor-not-allowed waiting:opacity-50',
        checked ? 'bg-accent' : 'bg-subtle', RING)}>
      <span aria-hidden="true" className={cx('absolute top-0.5 size-4 rounded-pill bg-white shadow-card transition-[left]', checked ? 'left-[18px]' : 'left-0.5')} />
    </button>
  </div>;
}
