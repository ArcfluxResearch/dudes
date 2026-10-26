import { useCallback, useContext, useId, useRef, useState } from 'react';
import type { ButtonHTMLAttributes, KeyboardEvent, ReactNode, Ref } from 'react';

import { BUTTON_BASE } from './Button.tsx';
import { RING_INSET, cx } from './cx.ts';
import { MenuClose } from './menuClose.ts';
import { useTopLayerPopover } from './popover.ts';

const ITEMS = '[role="menuitem"]:not([aria-disabled="true"])';

/**
 * A menu button and its `role="menu"` list. Focus moves into the list on open and comes back to the button on
 * Escape or after an item is chosen. The list is a child of the button's own wrapper, so it works inside a modal
 * `dialog`, and it shows in the top layer so the dialog cannot clip it.
 */
export function Menu({ label, trigger, children, className, align = 'end', ref }: {
  readonly label: string;
  readonly trigger: ReactNode;
  readonly children: ReactNode;
  readonly className?: string;
  readonly align?: 'start' | 'end';
  readonly ref?: Ref<HTMLButtonElement>;
}) {
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

  return <>
    <button ref={(node) => { button.current = node; if (typeof ref === 'function') ref(node); else if (ref) ref.current = node; }}
      type="button" aria-label={label} aria-haspopup="menu" aria-expanded={open} aria-controls={`${made}-menu`}
      onClick={() => { if (open) dismiss(); else enter('first'); }}
      onKeyDown={(event) => {
        if (event.key === 'ArrowDown') { event.preventDefault(); enter('first'); }
        else if (event.key === 'ArrowUp') { event.preventDefault(); enter('last'); }
      }}
      className={cx(BUTTON_BASE, 'size-8 border-line-strong bg-surface text-ink hover:bg-surface-2', className)}>{trigger}</button>
    <div ref={list} id={`${made}-menu`} role="menu" aria-label={label} popover="manual" tabIndex={-1}
      onKeyDown={keyOf}
      className="ui fixed inset-auto m-0 box-border min-w-48 animate-pop overflow-y-auto rounded-lg border border-line bg-surface p-1 font-sans text-ink shadow-pop">
      <MenuClose value={dismiss}>{children}</MenuClose>
    </div>
  </>;
}

/** One action in a menu. Choosing it closes the menu, gives focus back to the button, then runs `onSelect`. */
export function MenuItem({ onSelect, danger = false, icon, children, className, ref, ...rest }: {
  readonly onSelect: () => void;
  readonly danger?: boolean;
  readonly icon?: ReactNode;
  readonly children: ReactNode;
  readonly ref?: Ref<HTMLButtonElement>;
} & Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'role' | 'onClick' | 'children'>) {
  const dismiss = useContext(MenuClose);
  return <button ref={ref} type="button" role="menuitem" tabIndex={-1} {...rest}
    aria-disabled={rest['aria-disabled'] ?? (rest.disabled || undefined)}
    onClick={(event) => {
      if (rest.disabled === true || rest['aria-disabled'] === true || rest['aria-disabled'] === 'true') { event.preventDefault(); return; }
      dismiss();
      onSelect();
    }}
    className={cx('box-border flex h-8 w-full cursor-pointer items-center gap-2 rounded-md border-0 bg-transparent px-2 text-left font-sans text-base transition-colors hover:bg-surface-2 focus-visible:bg-surface-2 waiting:cursor-not-allowed waiting:opacity-50',
      danger ? 'text-danger' : 'text-ink', RING_INSET, className)}>
    {icon}{children}
  </button>;
}

export function MenuSeparator() {
  return <div role="separator" className="my-1 h-px bg-line" />;
}
