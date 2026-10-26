import { useId } from 'react';
import type { InputHTMLAttributes, ReactNode, Ref, TextareaHTMLAttributes } from 'react';

import { RING, cx } from './cx.ts';

export interface ControlProps {
  readonly id: string;
  readonly 'aria-describedby': string | undefined;
  readonly 'aria-invalid': true | undefined;
}

/**
 * A label, an optional hint and an optional error around one control. The control comes in as a function so it
 * receives the id its label points at and the ids its hint and error are tied to.
 */
export function Field({ label, hint, error, children, className, id: given }: {
  readonly id?: string;
  readonly label: ReactNode;
  readonly hint?: ReactNode;
  readonly error?: ReactNode;
  readonly children: (control: ControlProps) => ReactNode;
  readonly className?: string;
}) {
  const made = useId();
  const id = given ?? made;
  const hintId = hint === undefined ? undefined : `${id}-hint`;
  const errorId = error === undefined || error === null || error === false ? undefined : `${id}-error`;
  const described = [hintId, errorId].filter(Boolean).join(' ') || undefined;
  return <div className={cx('ui grid gap-1.5 font-sans', className)}>
    <label htmlFor={id} className="text-small font-medium text-ink">{label}</label>
    {children({ id, 'aria-describedby': described, 'aria-invalid': errorId === undefined ? undefined : true })}
    {hint === undefined ? null : <p id={hintId} className="m-0 text-small text-muted">{hint}</p>}
    {errorId === undefined ? null : <p id={errorId} role="alert" className="m-0 text-small font-medium text-danger">{error}</p>}
  </div>;
}

export const FIELD_BOX = cx(
  'ui box-border w-full min-w-0 rounded-md border border-line-strong bg-field px-3 font-sans text-base text-ink',
  'placeholder:text-subtle transition-colors hover:border-subtle invalid:border-danger',
  'disabled:cursor-not-allowed disabled:opacity-50', RING,
);

export function Input({ size = 'md', className, ref, ...rest }: Omit<InputHTMLAttributes<HTMLInputElement>, 'size'> & { readonly size?: 'sm' | 'md'; readonly ref?: Ref<HTMLInputElement> }) {
  return <input ref={ref} {...rest} className={cx(FIELD_BOX, size === 'sm' ? 'h-7' : 'h-8', className)} />;
}

export function Textarea({ className, ref, ...rest }: TextareaHTMLAttributes<HTMLTextAreaElement> & { readonly ref?: Ref<HTMLTextAreaElement> }) {
  return <textarea ref={ref} {...rest} className={cx(FIELD_BOX, 'min-h-16 resize-y py-2 leading-[1.45]', className)} />;
}
