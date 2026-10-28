// SPDX-License-Identifier: MIT OR Apache-2.0

import { Columns3, List, Plus, Search, Ticket } from 'lucide-react';
import { useEffect, useId, useMemo, useRef, useState } from 'react';
import type { KeyboardEvent, ReactNode } from 'react';

import { Dialog, ICON, Input, cn } from '../../ui/index.ts';
import { moveActive } from '../../ui/listNav.ts';
import type { FloorState } from '../floor/store.ts';
import { stateWord } from '../ticket/model.ts';
import { ticketActionsOpen } from '../ticket/TicketBoard.tsx';
import { listCommands } from './commands.ts';
import type { MenuRow } from './commands.ts';

const ICONS: Readonly<Record<MenuRow['kind'], ReactNode>> = {
  new: <Plus {...ICON} />, board: <Columns3 {...ICON} />, list: <List {...ICON} />, ticket: <Ticket {...ICON} />,
};

/**
 * Type to filter the commands and the tickets, then Enter or a click to run one. A command opens a view or the new-ticket
 * form. None of them gives a ticket to an agent or starts work.
 */
export function QuickMenu({ floor, onRun, onClose }: {
  readonly floor: FloorState;
  readonly onRun: (entry: MenuRow) => void;
  readonly onClose: () => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const [query, setQuery] = useState('');
  const [active, setActive] = useState(0);
  const listId = useId();
  const tickets = useMemo(() => floor.cards.map((card) => ({ id: card.id, title: card.title, detail: stateWord(card) })), [floor.cards]);
  const entries = useMemo(() => listCommands(query, tickets, ticketActionsOpen(floor)), [query, tickets, floor.ticket, floor.phase, floor.stale, floor.epoch, floor.snapshotEpoch]);
  const at = Math.min(active, entries.length - 1);
  useEffect(() => {
    const node = dialog.current;
    node?.showModal();
    return () => { node?.close(); };
  }, []);

  function key(event: KeyboardEvent<HTMLInputElement>): void {
    if (event.nativeEvent.isComposing) return;
    if (event.key === 'Enter') {
      const entry = entries[at];
      if (entry === undefined) return;
      event.preventDefault();
      onRun(entry);
      return;
    }
    const next = moveActive(entries, at, event.key === 'ArrowDown' || event.key === 'ArrowUp' ? event.key : '');
    if (next === null) return;
    event.preventDefault();
    setActive(next);
  }

  return <Dialog ref={dialog} size="md" data-command-menu className="command-menu mt-[12vh] mb-auto overflow-hidden" aria-label="Command menu"
    onCancel={(event) => { event.preventDefault(); onClose(); }}
    onKeyDown={(event) => { if (event.key === 'Escape') event.stopPropagation(); }}>
    <div className="flex flex-col gap-2 p-3">
      <div className="relative">
        <Search {...ICON} aria-hidden="true" className="pointer-events-none absolute top-2 left-2.5 text-subtle" />
        <Input type="text" role="combobox" autoFocus className="command-menu-input pl-8" aria-label="Search commands and tickets"
          placeholder="Type a command or a ticket title" maxLength={256} autoComplete="off" spellCheck={false}
          aria-expanded="true" aria-controls={listId} aria-autocomplete="list"
          aria-activedescendant={entries[at] === undefined ? undefined : `${listId}-${at}`}
          value={query} onChange={(event) => { setQuery(event.target.value); setActive(0); }} onKeyDown={key} />
      </div>
      <ul id={listId} role="listbox" aria-label="Commands and tickets" className="m-0 flex max-h-80 list-none flex-col gap-0.5 overflow-auto p-0">
        {entries.map((entry, index) => <li key={`${entry.kind}:${entry.id}`} id={`${listId}-${index}`} role="option" aria-selected={index === at}
          className={cn('command-menu-row flex min-w-0 cursor-pointer items-center gap-2 rounded-md px-2 py-2 text-base text-ink', index === at ? 'bg-accent-soft' : 'hover:bg-surface-2')}
          onClick={() => onRun(entry)}>
          <span className="shrink-0 text-muted" aria-hidden="true">{ICONS[entry.kind]}</span>
          <span className="min-w-0 flex-1 truncate">{entry.label}</span>
          {entry.kind === 'ticket' ? <span className="shrink-0 text-small text-muted">{entry.detail}</span> : null}
        </li>)}
      </ul>
      <p className="m-0 text-small text-muted" role="status">
        {entries.length === 0 ? 'No commands or tickets match.' : `${entries.length} ${entries.length === 1 ? 'result' : 'results'}`}
      </p>
    </div>
  </Dialog>;
}
