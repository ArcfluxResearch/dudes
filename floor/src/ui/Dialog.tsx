import type { DialogHTMLAttributes, Ref } from 'react';

import { cn } from './cn.ts';

export type DialogSize = 'sm' | 'md' | 'lg';

const WIDTH: Readonly<Record<DialogSize, string>> = {
  sm: 'w-[min(480px,calc(100vw-32px))]',
  md: 'w-[min(640px,calc(100vw-32px))]',
  lg: 'w-[min(864px,calc(100vw-32px))]',
};

/**
 * The native modal `dialog`, restyled. The caller still calls `showModal()` and `close()`, so the browser's focus
 * return, its Escape `cancel` and the owner's own focus rules stay as they are. A Menu or Select inside it is a child of
 * the dialog and shows in the top layer above it.
 */
export function Dialog({ size = 'md', className, ref, ...rest }: DialogHTMLAttributes<HTMLDialogElement> & { readonly size?: DialogSize; readonly ref?: Ref<HTMLDialogElement> }) {
  return <dialog ref={ref} data-ui="dialog" data-size={size} {...rest}
    className={cn('m-auto box-border max-h-[calc(100dvh-32px)] overflow-auto rounded-xl border border-line bg-surface p-0 font-sans text-ink shadow-sheet backdrop:bg-scrim/50', WIDTH[size], className)} />;
}
