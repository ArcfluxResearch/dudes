// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The Board's keyboard shortcuts: which key does what, when a key is left to the field being typed in,
// and the rows of the help sheet. The Floor's other shortcuts are Alt+Shift+F (focus an agent), the
// office's arrow keys when the office has focus, and Escape in each panel, drawer and dialog.

export type BoardCommand = 'search' | 'new' | 'next' | 'previous' | 'board' | 'list' | 'help';

export interface KeyLike {
  readonly key: string;
  readonly ctrlKey: boolean;
  readonly altKey: boolean;
  readonly metaKey: boolean;
  readonly isComposing?: boolean;
}

export interface TargetLike {
  readonly tagName?: string;
  readonly isContentEditable?: boolean;
  readonly closest?: (selector: string) => unknown;
}

const KEY_COMMAND: Readonly<Record<string, BoardCommand>> = Object.freeze({
  '/': 'search', c: 'new', j: 'next', k: 'previous', b: 'board', l: 'list', '?': 'help',
});

const FIELDS = new Set(['INPUT', 'TEXTAREA', 'SELECT']);
const WIDGETS = '[role="textbox"], [role="combobox"], [role="listbox"], [role="menu"]';

/** Whether the key is going to a field, or to a menu or list that reads letters itself. */
export function isTypingTarget(target: TargetLike | null): boolean {
  if (target === null) return false;
  if (target.isContentEditable === true || FIELDS.has(target.tagName ?? '')) return true;
  return typeof target.closest === 'function' && target.closest(WIDGETS) !== null;
}

/**
 * What a key does on the Board, or `null` when it is not ours. Nothing fires with Ctrl, Alt or Meta held, while
 * typing, or while an input method is composing. The arrow keys move the selection only when nothing has focus yet:
 * once a ticket has focus the Board's own arrow handling moves it.
 */
export function boardCommand(event: KeyLike, target: TargetLike | null): BoardCommand | null {
  if (event.ctrlKey || event.altKey || event.metaKey || event.isComposing === true) return null;
  if (isTypingTarget(target)) return null;
  if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
    if (target !== null && target.tagName !== 'BODY') return null;
    return event.key === 'ArrowDown' ? 'next' : 'previous';
  }
  return Object.hasOwn(KEY_COMMAND, event.key) ? KEY_COMMAND[event.key]! : null;
}

/** Whether this browser runs on an Apple system, where Cmd stands where Ctrl does elsewhere. */
export function isMac(): boolean {
  return typeof navigator !== 'undefined' && /Mac|iPhone|iPad|iPod/.test(navigator.platform);
}

/** Cmd+K on macOS and Ctrl+K elsewhere, with no other modifier. Ctrl+K on a Mac is a text field's own key. */
export function opensCommandMenu(event: KeyLike & { readonly shiftKey: boolean; readonly repeat?: boolean }, mac: boolean): boolean {
  if (event.key.toLowerCase() !== 'k' || event.altKey || event.shiftKey || event.repeat === true) return false;
  return mac ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey;
}

export interface ShortcutRow {
  readonly keys: readonly string[];
  readonly label: string;
}

/** The help sheet's rows, in the order they are read. */
export function shortcutRows(mac: boolean): readonly ShortcutRow[] {
  return [
    { keys: ['/'], label: 'Search tickets' },
    { keys: ['c'], label: 'New ticket' },
    { keys: ['j', '↓'], label: 'Next ticket' },
    { keys: ['k', '↑'], label: 'Previous ticket' },
    { keys: ['Enter'], label: 'Open the selected ticket' },
    { keys: ['Esc'], label: 'Close the top layer' },
    { keys: ['b'], label: 'Show the Board' },
    { keys: ['l'], label: 'Show the List' },
    { keys: [mac ? '⌘K' : 'Ctrl+K'], label: 'Command menu' },
    { keys: ['?'], label: 'Show this sheet' },
    { keys: [mac ? '⌘↵' : 'Ctrl+↵'], label: 'Create the ticket, in the New ticket dialog' },
    { keys: ['p', 'd', 'a'], label: 'Open Priority, Done when or Agent, in the New ticket dialog' },
  ];
}
