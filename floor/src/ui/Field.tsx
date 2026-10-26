import { useId } from 'react';
import type { ComponentPropsWithRef, ReactNode } from 'react';

import { cn } from './cn.ts';
import { FieldContext, useFieldControl } from './fieldContext.ts';
import { RING } from './look.ts';

export interface FieldProps {
  readonly label: ReactNode;
  readonly hint?: ReactNode;
  /** The error message. It marks the control invalid and is tied to it with aria-describedby. */
  readonly error?: ReactNode;
  /** The control's id. One is made when it is left out. */
  readonly id?: string;
  readonly className?: string;
  /** One control: Input, Textarea or Select. It takes its id, description and invalid state from the field. */
  readonly children: ReactNode;
}

/** A label, a control, a hint and an error, tied together without the call site wiring any ids. */
export function Field({ label, hint, error, id, className, children }: FieldProps) {
  const auto = useId();
  const control = id ?? auto;
  const hintId = `${control}-hint`;
  const errorId = `${control}-error`;
  const described = [hint === undefined ? null : hintId, error === undefined ? null : errorId].filter(Boolean).join(' ');
  return (
    <div data-ui="field" data-invalid={error === undefined ? undefined : 'true'} className={cn('flex flex-col gap-1.5 font-sans', className)}>
      <label htmlFor={control} className="text-small font-medium text-ink">{label}</label>
      <FieldContext value={{ id: control, describedBy: described === '' ? undefined : described, invalid: error === undefined ? undefined : true }}>
        {children}
      </FieldContext>
      {hint === undefined ? null : <p id={hintId} className="m-0 text-small text-muted">{hint}</p>}
      {error === undefined ? null : <p id={errorId} role="alert" className="m-0 text-small text-danger">{error}</p>}
    </div>
  );
}

/** The box every text control draws: edge, fill, hover, focus, invalid and disabled. */
export const FIELD_BOX = cn(
  'box-border w-full rounded-md border border-line-strong bg-field px-3 font-sans text-base text-ink',
  'placeholder:text-subtle transition-[color,background-color,border-color,opacity] hover:border-muted invalid:border-danger',
  'focus-visible:border-focus disabled:cursor-not-allowed disabled:bg-surface-2 disabled:opacity-60',
  RING,
);

function joined(own: string | undefined, field: string | undefined): string | undefined {
  const both = [own, field].filter(Boolean).join(' ');
  return both === '' ? undefined : both;
}

export function Input({ className, ...rest }: ComponentPropsWithRef<'input'>) {
  const field = useFieldControl();
  return <input data-ui="input" {...rest} id={rest.id ?? field.id}
    aria-describedby={joined(rest['aria-describedby'], field.describedBy)} aria-invalid={rest['aria-invalid'] ?? field.invalid}
    className={cn(FIELD_BOX, 'h-8', className)} />;
}

export function Textarea({ className, ...rest }: ComponentPropsWithRef<'textarea'>) {
  const field = useFieldControl();
  return <textarea data-ui="textarea" {...rest} id={rest.id ?? field.id}
    aria-describedby={joined(rest['aria-describedby'], field.describedBy)} aria-invalid={rest['aria-invalid'] ?? field.invalid}
    className={cn(FIELD_BOX, 'min-h-20 resize-y py-1.5 leading-normal', className)} />;
}
