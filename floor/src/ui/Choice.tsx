import type { InputHTMLAttributes, ReactNode, Ref } from 'react';

import { cn } from './cn.ts';
import { RING } from './look.ts';

const MARK = cn(
  'm-0 box-border size-4 shrink-0 cursor-pointer appearance-none border border-line-strong bg-field',
  'transition-[color,background-color,border-color,opacity] hover:border-muted',
  'checked:border-accent checked:bg-accent disabled:cursor-not-allowed disabled:opacity-50 invalid:border-danger',
  RING,
);

const SHAPE = {
  radio: 'rounded-pill checked:shadow-[inset_0_0_0_3px_var(--ui-surface)]',
  checkbox: cn('relative rounded-sm',
    'checked:after:absolute checked:after:top-px checked:after:left-[4px] checked:after:h-[8px] checked:after:w-[4px]',
    'checked:after:rotate-45 checked:after:border-r-2 checked:after:border-b-2 checked:after:border-solid checked:after:border-accent-fg checked:after:content-[""]'),
} as const;

export type ChoiceProps = Omit<InputHTMLAttributes<HTMLInputElement>, 'type' | 'children' | 'className'> & {
  readonly type: 'radio' | 'checkbox';
  /** What the option is called. */
  readonly label: ReactNode;
  /** A line under the label that says what choosing it means. */
  readonly note?: ReactNode;
  /** Shown at the right edge, for a short fact such as a count. */
  readonly aside?: ReactNode;
  /** Draws the option as a bare row with no edge, for a short list of plain options. */
  readonly plain?: boolean;
  readonly className?: string;
  readonly ref?: Ref<HTMLInputElement>;
};

/**
 * One option of a radio group or one checkbox, as a row that can be pressed anywhere. The native input stays the control,
 * so the keyboard, the form name and the accessible name are the browser's own, and only its look is replaced.
 */
export function Choice({ type, label, note, aside, plain = false, className, ref, ...input }: ChoiceProps) {
  return (
    <label data-ui="choice" data-type={type}
      className={cn('box-border flex cursor-pointer items-start gap-3 rounded-md font-sans text-base text-ink',
        plain ? 'py-1' : 'border border-line bg-surface p-3 transition-colors hover:bg-surface-2 has-checked:border-accent has-checked:bg-accent-soft',
        'has-disabled:cursor-not-allowed has-disabled:opacity-60', className)}>
      <input ref={ref} type={type} {...input} className={cn(MARK, SHAPE[type], 'mt-px')} />
      <span className="flex min-w-0 flex-1 flex-col gap-0.5">
        <span className="font-medium wrap-anywhere">{label}</span>
        {note === undefined ? null : <span className="text-small text-muted">{note}</span>}
      </span>
      {aside === undefined ? null : <span className="shrink-0 text-small text-muted">{aside}</span>}
    </label>
  );
}
