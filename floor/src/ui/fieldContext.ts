import { createContext, useContext } from 'react';

export interface FieldControl {
  readonly id: string | undefined;
  readonly describedBy: string | undefined;
  readonly invalid: boolean | undefined;
}

const NO_FIELD: FieldControl = { id: undefined, describedBy: undefined, invalid: undefined };

export const FieldContext = createContext<FieldControl>(NO_FIELD);

/** What a control inside a Field takes from it: the id its label points at, and the hint and error that describe it. */
export function useFieldControl(): FieldControl {
  return useContext(FieldContext);
}
