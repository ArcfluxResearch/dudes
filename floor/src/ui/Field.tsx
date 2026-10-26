// SPDX-License-Identifier: MIT OR Apache-2.0

import { useId } from 'react';
import type { ComponentPropsWithRef, ReactNode } from 'react';

import { cx } from './cx.ts';
import { FieldContext, useFieldControl } from './fieldContext.ts';
import { SearchIcon } from './icons.tsx';
import './base.css';
import './Field.css';

export { useFieldControl } from './fieldContext.ts';

export interface FieldProps {
  readonly label: ReactNode;
  readonly hint?: ReactNode;
  /** The error message. It marks the control invalid and is tied to it with aria-describedby. */
  readonly error?: ReactNode;
  /** The control's id. Pass the id the control is known by elsewhere; one is made when it is left out. */
  readonly id?: string;
  readonly className?: string;
  /** One control: Input, Textarea, SearchInput or Select. It takes its id, description and invalid state from the field. */
  readonly children: ReactNode;
}

export function Field({ label, hint, error, id, className, children }: FieldProps) {
  const auto = useId();
  const control = id ?? auto;
  const hintId = `${control}-hint`;
  const errorId = `${control}-error`;
  const described = [hint === undefined ? null : hintId, error === undefined ? null : errorId].filter(Boolean).join(' ');
  return (
    <div className={cx(className, 'ui-field')} data-invalid={error === undefined ? undefined : 'true'}>
      <label htmlFor={control}>{label}</label>
      <FieldContext value={{ id: control, describedBy: described === '' ? undefined : described, invalid: error === undefined ? undefined : true }}>
        {children}
      </FieldContext>
      {hint === undefined ? null : <p className="ui-field-hint" id={hintId}>{hint}</p>}
      {error === undefined ? null : <p className="ui-field-error" id={errorId} role="alert">{error}</p>}
    </div>
  );
}

function joined(own: string | undefined, field: string | undefined): string | undefined {
  const both = [own, field].filter(Boolean).join(' ');
  return both === '' ? undefined : both;
}

export function Input({ className, ...rest }: ComponentPropsWithRef<'input'>) {
  const field = useFieldControl();
  return <input className={className} {...rest} id={rest.id ?? field.id}
    aria-describedby={joined(rest['aria-describedby'], field.describedBy)} aria-invalid={rest['aria-invalid'] ?? field.invalid}
    data-ui="input" />;
}

export function Textarea({ className, ...rest }: ComponentPropsWithRef<'textarea'>) {
  const field = useFieldControl();
  return <textarea className={className} {...rest} id={rest.id ?? field.id}
    aria-describedby={joined(rest['aria-describedby'], field.describedBy)} aria-invalid={rest['aria-invalid'] ?? field.invalid}
    data-ui="input" data-multiline="" />;
}

/** A search box: the input with a magnifier inside its left edge. */
export function SearchInput({ className, ...rest }: ComponentPropsWithRef<'input'>) {
  const field = useFieldControl();
  return (
    <span className="ui-search">
      <SearchIcon />
      <input type="search" className={className} {...rest} id={rest.id ?? field.id}
        aria-describedby={joined(rest['aria-describedby'], field.describedBy)} data-ui="input" />
    </span>
  );
}
