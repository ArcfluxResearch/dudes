import type { ButtonHTMLAttributes, HTMLAttributes, Ref } from 'react';

import { RING_INSET, cx } from './cx.ts';

/** Wraps `role="tablist"`. Arrow-key movement stays with the owner, which knows which tabs exist and what a move means. */
export function TabList({ className, ref, ...rest }: HTMLAttributes<HTMLDivElement> & { readonly ref?: Ref<HTMLDivElement> }) {
  return <div ref={ref} role="tablist" {...rest}
    className={cx('ui flex gap-1 overflow-x-auto border-b border-line font-sans', className)} />;
}

/** One tab. The owner passes `aria-selected`, and the style follows it. */
export function Tab({ className, ref, ...rest }: {
  readonly 'aria-selected': boolean;
  readonly ref?: Ref<HTMLButtonElement>;
} & Omit<ButtonHTMLAttributes<HTMLButtonElement>, 'role' | 'aria-selected'>) {
  const selected = rest['aria-selected'];
  return <button ref={ref} type="button" role="tab" {...rest}
    className={cx('box-border h-9 shrink-0 cursor-pointer rounded-t-md border-0 border-b-2 bg-transparent px-3 font-sans text-base font-medium transition-colors',
      selected ? 'border-accent text-ink' : 'border-transparent text-muted hover:text-ink', RING_INSET, className)} />;
}
