// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ComponentPropsWithRef, KeyboardEvent, ReactNode } from 'react';

import './base.css';
import './Tabs.css';

/** The row of tabs. Arrow keys, Home and End move to a tab and select it; only the selected tab is in the tab order. */
export function TabList({ className, onKeyDown, children, ...rest }: ComponentPropsWithRef<'div'> & { readonly children: ReactNode }) {
  function move(event: KeyboardEvent<HTMLDivElement>): void {
    onKeyDown?.(event);
    if (event.defaultPrevented || event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
    const tabs = [...event.currentTarget.querySelectorAll<HTMLElement>('[role="tab"]:not([aria-disabled="true"])')];
    const at = tabs.indexOf(event.target as HTMLElement);
    if (at === -1) return;
    const next = event.key === 'ArrowRight' ? tabs[(at + 1) % tabs.length]
      : event.key === 'ArrowLeft' ? tabs[(at + tabs.length - 1) % tabs.length]
        : event.key === 'Home' ? tabs[0] : event.key === 'End' ? tabs.at(-1) : undefined;
    if (next === undefined) return;
    event.preventDefault();
    next.focus();
    next.click();
  }
  return <div className={className} role="tablist" onKeyDown={move} {...rest} data-ui="tabs">{children}</div>;
}

export interface TabProps extends Omit<ComponentPropsWithRef<'button'>, 'role'> {
  readonly selected: boolean;
}

export function Tab({ selected, className, children, ...rest }: TabProps) {
  return (
    <button type="button" role="tab" className={className} {...rest} aria-selected={selected}
      tabIndex={selected ? 0 : -1}>{children}</button>
  );
}
