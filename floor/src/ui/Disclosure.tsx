// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ComponentPropsWithRef, ReactNode } from 'react';

import './base.css';
import './Disclosure.css';

export interface DisclosureProps extends Omit<ComponentPropsWithRef<'details'>, 'summary'> {
  readonly summary: ReactNode;
}

/** A native details element. The chevron is drawn by the stylesheet, so the markup is the same as a bare details. */
export function Disclosure({ summary, children, ...rest }: DisclosureProps) {
  return (
    <details {...rest} data-ui="disclosure">
      <summary>{summary}</summary>
      {children}
    </details>
  );
}
