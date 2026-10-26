// SPDX-License-Identifier: MIT OR Apache-2.0

import { useCallback, useId, useLayoutEffect, useRef, useState } from 'react';
import type { KeyboardEvent, MouseEvent, ReactNode } from 'react';

import { cx } from './cx.ts';
import { IconButton } from './Button.tsx';
import { MoreIcon } from './icons.tsx';
import { useAnchoredPopover } from './popover.ts';
import './popover.css';
import './Menu.css';

export interface MenuProps {
  /** The trigger's accessible name and the menu's. */
  readonly label: string;
  readonly children: ReactNode;
  readonly className?: string;
  readonly align?: 'start' | 'end';
  /** A different trigger glyph than the three dots. */
  readonly icon?: ReactNode;
}

function items(panel: HTMLElement | null): HTMLElement[] {
  return panel === null ? [] : [...panel.querySelectorAll<HTMLElement>('[role="menuitem"]:not([aria-disabled="true"])')];
}

/** A menu button. Arrow keys, Home, End and Escape work; closing returns focus to the button. */
export function Menu({ label, children, className, align = 'end', icon }: MenuProps) {
  const [open, setOpen] = useState(false);
  const trigger = useRef<HTMLButtonElement>(null);
  const panel = useRef<HTMLDivElement>(null);
  const id = useId();
  const close = useCallback((returnFocus: boolean) => {
    setOpen(false);
    if (returnFocus) trigger.current?.focus();
  }, []);
  useAnchoredPopover(open, trigger, panel, close, align);
  const focusAt = useRef<'first' | 'last'>('first');
  useLayoutEffect(() => {
    if (!open) return;
    const list = items(panel.current);
    (focusAt.current === 'last' ? list.at(-1) : list[0])?.focus();
  }, [open]);

  function openAt(at: 'first' | 'last'): void {
    focusAt.current = at;
    setOpen(true);
  }

  function onTriggerKey(event: KeyboardEvent<HTMLButtonElement>): void {
    if (event.key === 'ArrowDown') { event.preventDefault(); openAt('first'); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); openAt('last'); }
    else if (event.key === 'Escape' && open) { event.preventDefault(); event.stopPropagation(); close(true); }
  }

  function onPanelKey(event: KeyboardEvent<HTMLDivElement>): void {
    const list = items(panel.current);
    const at = list.indexOf(event.target as HTMLElement);
    let next: HTMLElement | undefined;
    switch (event.key) {
      case 'ArrowDown': next = list[(at + 1) % list.length]; break;
      case 'ArrowUp': next = list[(at - 1 + list.length) % list.length]; break;
      case 'Home': next = list[0]; break;
      case 'End': next = list.at(-1); break;
      case 'Escape': event.preventDefault(); event.stopPropagation(); close(true); return;
      case 'Tab': close(false); return;
      default: return;
    }
    event.preventDefault();
    event.stopPropagation();
    next?.focus();
  }

  // An item's own click runs first, since it is the target; the menu then closes and hands focus back.
  function onPanelClick(event: MouseEvent<HTMLDivElement>): void {
    const item = (event.target as HTMLElement).closest('[role="menuitem"]');
    if (item !== null && item.getAttribute('aria-disabled') !== 'true') close(true);
  }

  return (
    <div className={cx(className, 'ui-menu')}>
      <IconButton ref={trigger} variant="ghost" label={label} icon={icon ?? <MoreIcon />} aria-haspopup="menu"
        aria-expanded={open} aria-controls={`${id}-menu`}
        onClick={() => { if (open) close(false); else openAt('first'); }} onKeyDown={onTriggerKey} />
      <div ref={panel} className="ui-pop ui-menu-panel" id={`${id}-menu`} role="menu" aria-label={label}
        popover="manual" onKeyDown={onPanelKey} onClick={onPanelClick}>
        {children}
      </div>
    </div>
  );
}

export interface MenuItemProps {
  readonly children: ReactNode;
  readonly onSelect: () => void;
  readonly icon?: ReactNode;
  readonly danger?: boolean;
  readonly disabled?: boolean;
  readonly title?: string;
  readonly className?: string;
  readonly id?: string;
  readonly [data: `data-${string}`]: string | undefined;
}

export function MenuItem({ children, onSelect, icon, danger = false, disabled = false, className, ...rest }: MenuItemProps) {
  return (
    <button type="button" role="menuitem" tabIndex={-1} aria-disabled={disabled || undefined}
      className={cx(className, 'ui-pop-item', danger && 'ui-pop-item--danger')} {...rest}
      onClick={() => { if (!disabled) onSelect(); }}>
      {icon}{children}
    </button>
  );
}

export function MenuSeparator() {
  return <hr className="ui-menu-separator" role="separator" />;
}

/** A line of explanation inside a menu, for something that cannot be offered and why. */
export function MenuNote({ children }: { readonly children: ReactNode }) {
  return <div className="ui-menu-note" role="none">{children}</div>;
}
