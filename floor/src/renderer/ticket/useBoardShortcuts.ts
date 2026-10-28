// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef } from 'react';
import type { RefObject } from 'react';

import type { BoardLayout } from './browse.ts';
import { boardCommand } from './shortcuts.ts';

interface Hooks {
  readonly active: boolean;
  readonly root: RefObject<HTMLElement | null>;
  readonly newButton: RefObject<HTMLElement | null>;
  readonly canCreate: boolean;
  readonly onNew: (opener: HTMLElement) => void;
  readonly onLayout: (layout: BoardLayout) => void;
  readonly onHelp: () => void;
}

const TICKET = '[data-card-id]:not(:disabled)';

/** Move the keyboard selection, which is the focus ring, one ticket along the Board's reading order. */
function moveSelection(root: HTMLElement, step: 1 | -1): void {
  const tickets = [...root.querySelectorAll<HTMLElement>(TICKET)].filter((node) => node.offsetParent !== null);
  if (tickets.length === 0) return;
  const at = tickets.findIndex((node) => node === document.activeElement);
  const next = at === -1 ? 0 : Math.min(tickets.length - 1, Math.max(0, at + step));
  tickets[next]!.focus();
}

/** The Board's keyboard shortcuts, off while a dialog is open, a field has the keys, or `active` is false. */
export function useBoardShortcuts(hooks: Hooks): void {
  const latest = useRef(hooks);
  latest.current = hooks;
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const { active, root, newButton, canCreate, onNew, onLayout, onHelp } = latest.current;
      const board = root.current;
      if (!active || board === null || event.defaultPrevented || document.querySelector('dialog:modal') !== null) return;
      const command = boardCommand(event, event.target instanceof Element ? event.target : null);
      if (command === null) return;
      if (command === 'new' && (!canCreate || newButton.current === null)) return;
      event.preventDefault();
      switch (command) {
        case 'search': board.querySelector<HTMLElement>('.board-search')?.focus(); break;
        case 'new': onNew(newButton.current!); break;
        case 'next': moveSelection(board, 1); break;
        case 'previous': moveSelection(board, -1); break;
        case 'board': onLayout('board'); break;
        case 'list': onLayout('list'); break;
        case 'help': onHelp(); break;
      }
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, []);
}
