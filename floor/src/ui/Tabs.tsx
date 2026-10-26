import type { ButtonHTMLAttributes, HTMLAttributes, KeyboardEvent, ReactNode, Ref } from 'react';

import { cn } from './cn.ts';
import { RING_INSET } from './look.ts';

/**
 * A row of tabs. Arrow keys, Home and End move between them and select the one reached, and only the selected tab is in
 * the tab order, so Tab leaves the row.
 */
export function TabList({ label, children, className, ref, ...rest }: {
  readonly label: string;
  readonly children: ReactNode;
  readonly className?: string;
  readonly ref?: Ref<HTMLDivElement>;
} & Omit<HTMLAttributes<HTMLDivElement>, 'role' | 'aria-label' | 'children' | 'onKeyDown'>) {
  function keyOf(event: KeyboardEvent<HTMLDivElement>): void {
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    const tabs = [...event.currentTarget.querySelectorAll<HTMLElement>('[role="tab"]:not([aria-disabled="true"])')];
    const at = tabs.indexOf(document.activeElement as HTMLElement);
    if (at === -1) return;
    const to = event.key === 'ArrowRight' ? tabs[(at + 1) % tabs.length]
      : event.key === 'ArrowLeft' ? tabs[(at - 1 + tabs.length) % tabs.length]
        : event.key === 'Home' ? tabs[0]
          : event.key === 'End' ? tabs[tabs.length - 1] : undefined;
    if (to === undefined) return;
    event.preventDefault();
    to.focus();
    to.click();
  }
  return <div ref={ref} role="tablist" aria-label={label} data-ui="tab-list" {...rest} onKeyDown={keyOf}
    className={cn('flex gap-1 overflow-x-auto border-b border-line font-sans', className)}>{children}</div>;
}

export type TabProps = {
  readonly selected: boolean;
  readonly children: ReactNode;
  readonly ref?: Ref<HTMLButtonElement>;
} & Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'role' | 'children'>;

/** The selected state is the `aria-selected` attribute, so a screen reader and the stylesheet read one source. */
export function Tab({ selected, children, className, type = 'button', ref, ...rest }: TabProps) {
  return <button ref={ref} type={type} role="tab" aria-selected={selected} tabIndex={selected ? 0 : -1} data-ui="tab"
    data-state={selected ? 'selected' : 'idle'} {...rest}
    className={cn('-mb-px box-border h-9 shrink-0 cursor-pointer whitespace-nowrap border-0 border-b-2 border-transparent bg-transparent px-3',
      'font-sans text-base font-medium text-muted transition-[color,background-color,border-color,opacity] hover:text-ink',
      'disabled:cursor-not-allowed disabled:opacity-50 selected:border-accent selected:text-ink', RING_INSET, className)}>{children}</button>;
}
