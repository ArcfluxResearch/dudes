// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ComponentPropsWithRef } from 'react';

import './Badge.css';

export type StateKind = 'needs-you' | 'ready' | 'working' | 'waiting' | 'done' | 'idle';

/** One badge per state word. The colour never carries the meaning alone: the word is always drawn. */
export function Badge({ state, ...rest }: ComponentPropsWithRef<'span'> & { readonly state: StateKind }) {
  return <span {...rest} data-state={state} data-ui="badge" />;
}
