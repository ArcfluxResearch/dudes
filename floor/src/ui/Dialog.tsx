// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ComponentPropsWithRef } from 'react';

import './base.css';
import './Dialog.css';

/**
 * The page's native modal dialog, restyled. The caller still opens it with `showModal()`, so the browser keeps the focus trap,
 * Escape and the hand-back of focus to whatever opened it. Menus and selects inside it are its children, never portalled out.
 */
export function Dialog(props: ComponentPropsWithRef<'dialog'>) {
  return <dialog {...props} data-ui="dialog" data-ui-scope="" />;
}
