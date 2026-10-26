import type { DialogHTMLAttributes, Ref } from 'react';

import { cx } from './cx.ts';

/**
 * The native modal `dialog`, restyled. The caller still calls `showModal()` and closes it, so focus return and the
 * Escape rule stay with the owner. A popover opened inside it is a child of the dialog and shows in the top layer.
 */
export type DialogSize = 'sm' | 'md' | 'lg';

const WIDTH: Readonly<Record<DialogSize, string>> = {
  sm: 'w-[min(480px,calc(100vw-32px))]',
  md: 'w-[min(640px,calc(100vw-32px))]',
  lg: 'w-[min(864px,calc(100vw-32px))]',
};

export function Dialog({ size = 'md', className, ref, ...rest }: DialogHTMLAttributes<HTMLDialogElement> & { readonly size?: DialogSize; readonly ref?: Ref<HTMLDialogElement> }) {
  return <dialog ref={ref} {...rest}
    className={cx('ui m-auto box-border max-h-[calc(100dvh-32px)] overflow-auto rounded-xl border border-line bg-surface p-0 font-sans text-ink shadow-sheet backdrop:bg-black/50', WIDTH[size], className)} />;
}
