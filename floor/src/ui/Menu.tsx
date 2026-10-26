import { MoreHorizontal } from 'lucide-react';
import { useCallback, useContext, useId, useRef, useState } from 'react';
import type { ButtonHTMLAttributes, KeyboardEvent, ReactNode, Ref } from 'react';

import { buttonClass } from './Button.tsx';
import { cn } from './cn.ts';
import { ICON, RING_INSET } from './look.ts';
import { MenuClose } from './menuClose.ts';
import { useTopLayerPopover } from './popover.ts';

const ITEMS = '[role="menuitem"]:not([aria-disabled="true"])';

export interface MenuProps {
  /** The accessible name of the button and of the list. */
  readonly label: string;
  /** What the button shows. Defaults to the three dots. */
  readonly trigger?: ReactNode;
  readonly children: ReactNode;
  readonly className?: string;
  readonly align?: 'start' | 'end';
  readonly ref?: Ref<HTMLButtonElement>;
}

/**
 * A menu button and its `role="menu"` list. Focus moves into the list on open and comes back to the button on Escape or
 * after an item is chosen. The list is drawn beside the button as a child of the component, so it works inside a modal
 * `dialog`, and it shows in the top layer so the dialog cannot clip it.
 */
export function Menu({ label, trigger = <MoreHorizontal {...ICON} />, children, className, align = 'end', ref }: MenuProps) {
  const made = useId();
  const [open, setOpen] = useState(false);
  const button = useRef<HTMLButtonElement | null>(null);
  const list = useRef<HTMLDivElement | null>(null);
  const away = useCallback(() => setOpen(false), []);
  useTopLayerPopover(open, button, list, away, false, align === 'end');

  function items(): HTMLElement[] {
    return [...(list.current?.querySelectorAll<HTMLElement>(ITEMS) ?? [])];
  }

  function enter(which: 'first' | 'last'): void {
    setOpen(true);
    requestAnimationFrame(() => {
      const found = items();
      (which === 'first' ? found[0] : found[found.length - 1])?.focus();
    });
  }

  function dismiss(): void {
    setOpen(false);
    button.current?.focus();
  }

  function keyOf(event: KeyboardEvent<HTMLDivElement>): void {
    if (event.altKey || event.ctrlKey || event.metaKey) return;
    const found = items();
    const at = found.indexOf(document.activeElement as HTMLElement);
    const go = (to: HTMLElement | undefined) => { event.preventDefault(); to?.focus(); };
    if (event.key === 'ArrowDown') go(found[(at + 1) % found.length]);
    else if (event.key === 'ArrowUp') go(found[(at - 1 + found.length) % found.length]);
    else if (event.key === 'Home') go(found[0]);
    else if (event.key === 'End') go(found[found.length - 1]);
    else if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      dismiss();
    } else if (event.key === 'Tab') dismiss();
    else if (event.key.length === 1 && /\S/.test(event.key)) {
      const next = [...found.slice(at + 1), ...found.slice(0, at + 1)]
        .find((item) => (item.textContent ?? '').trim().toLowerCase().startsWith(event.key.toLowerCase()));
      if (next !== undefined) go(next);
    }
  }

  function triggerKey(event: KeyboardEvent<HTMLButtonElement>): void {
    if (event.key === 'ArrowDown') { event.preventDefault(); enter('first'); }
    else if (event.key === 'ArrowUp') { event.preventDefault(); enter('last'); }
  }

  return <>
    <button ref={(node) => { button.current = node; if (typeof ref === 'function') ref(node); else if (ref) ref.current = node; }}
      type="button" aria-label={label} title={label} aria-haspopup="menu" aria-expanded={open} aria-controls={`${made}-menu`}
      data-ui="menu-button" data-variant="ghost" data-size="md" data-open={open || undefined}
      onClick={() => { if (open) dismiss(); else enter('first'); }}
      onKeyDown={triggerKey}
      className={buttonClass('ghost', 'md', true, cn(open && 'bg-surface-2 text-ink', className))}>{trigger}</button>
    <div ref={list} id={`${made}-menu`} role="menu" aria-label={label} popover="manual" tabIndex={-1} data-ui="menu"
      onKeyDown={keyOf}
      className="fixed inset-auto m-0 box-border min-w-48 animate-pop overflow-y-auto rounded-lg border border-line bg-surface p-1 font-sans text-ink shadow-pop">
      <MenuClose value={dismiss}>{children}</MenuClose>
    </div>
  </>;
}

export type MenuItemProps = {
  /** Runs after the menu has closed and focus is back on the button. */
  readonly onSelect: () => void;
  readonly danger?: boolean;
  readonly icon?: ReactNode;
  readonly children: ReactNode;
  readonly ref?: Ref<HTMLButtonElement>;
} & Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'role' | 'onClick' | 'children'>;

export function MenuItem({ onSelect, danger = false, icon, children, className, ref, ...rest }: MenuItemProps) {
  const dismiss = useContext(MenuClose);
  const blocked = rest.disabled === true || rest['aria-disabled'] === true || rest['aria-disabled'] === 'true';
  return <button ref={ref} type="button" role="menuitem" tabIndex={-1} data-ui="menu-item" data-variant={danger ? 'danger' : 'default'} {...rest}
    aria-disabled={blocked || undefined}
    onClick={(event) => {
      if (blocked) { event.preventDefault(); return; }
      dismiss();
      onSelect();
    }}
    className={cn('box-border flex h-8 w-full cursor-pointer items-center gap-2 rounded-md border-0 bg-transparent px-2 text-left font-sans text-base',
      'transition-[color,background-color,border-color,opacity] hover:bg-surface-2 focus-visible:bg-surface-2 waiting:cursor-not-allowed waiting:opacity-50',
      danger ? 'text-danger' : 'text-ink', RING_INSET, className)}>
    {icon}{children}
  </button>;
}

export function MenuSeparator() {
  return <div role="separator" data-ui="menu-separator" className="my-1 h-px bg-line" />;
}
