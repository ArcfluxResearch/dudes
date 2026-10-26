// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ComponentPropsWithRef, ReactNode } from 'react';

import { cx } from './cx.ts';
import './Card.css';

export function Card({ className, ...rest }: ComponentPropsWithRef<'section'>) {
  return <section className={cx(className, 'ui-card')} {...rest} />;
}

export interface PanelProps extends Omit<ComponentPropsWithRef<'section'>, 'title'> {
  readonly title: ReactNode;
  readonly titleId?: string;
  /** Controls drawn at the right of the title. */
  readonly actions?: ReactNode;
}

/** A card with a title row. The title is the section's accessible name. */
export function Panel({ title, titleId, actions, className, children, ...rest }: PanelProps) {
  return (
    <section className={cx(className, 'ui-card', 'ui-panel')} aria-labelledby={titleId} {...rest}>
      <header className="ui-panel-head">
        <h3 className="ui-panel-title" id={titleId}>{title}</h3>
        {actions === undefined ? null : <div className="ui-panel-actions">{actions}</div>}
      </header>
      <div className="ui-panel-body">{children}</div>
    </section>
  );
}
