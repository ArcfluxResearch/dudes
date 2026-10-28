// SPDX-License-Identifier: MIT OR Apache-2.0

import { Columns3, Keyboard, List, Search, X } from 'lucide-react';
import type { KeyboardEvent, ReactNode } from 'react';

import { Button, ICON, IconButton, Input, Select } from '../../ui/index.ts';
import { GROUP_NAMES, STATUS_CHOICES, isFiltered } from './browse.ts';
import type { BoardLayout, BrowseView, Choices, GroupBy } from './browse.ts';

const LAYOUTS: readonly { readonly key: BoardLayout; readonly label: string; readonly icon: ReactNode; readonly shortcut: string }[] = Object.freeze([
  { key: 'board', label: 'Board', icon: <Columns3 {...ICON} />, shortcut: 'b' },
  { key: 'list', label: 'List', icon: <List {...ICON} />, shortcut: 'l' },
]);

const FILTER = 'w-40 max-[800px]:w-full max-[800px]:min-w-0';

/** Layout, search, filters and grouping for the Board, and how many tickets they leave. */
export function BoardToolbar({ view, choices, total, shown, onChange, onClear, onHelp }: {
  readonly view: BrowseView;
  readonly choices: Choices;
  readonly total: number;
  readonly shown: number;
  readonly onChange: (patch: Partial<BrowseView>) => void;
  readonly onClear: () => void;
  readonly onHelp: () => void;
}) {
  const filtered = isFiltered(view);
  function searchKey(event: KeyboardEvent<HTMLInputElement>): void {
    if (event.key !== 'Escape' || view.search === '') return;
    event.preventDefault();
    onChange({ search: '' });
  }
  return <div className="board-toolbar flex flex-wrap items-center gap-2 max-[800px]:grid max-[800px]:grid-cols-2" role="group" aria-label="Show tickets">
    <div role="group" aria-label="Layout" className="inline-flex gap-0.5 rounded-md bg-surface-2 p-0.5 max-[800px]:col-span-2 max-[800px]:justify-self-start">
      {LAYOUTS.map(({ key, label, icon, shortcut }) => <Button key={key} variant="ghost" size="sm" icon={icon}
        className="board-layout aria-pressed:bg-surface aria-pressed:text-ink aria-pressed:shadow-card" data-layout={key} aria-keyshortcuts={shortcut}
        aria-pressed={view.layout === key} onClick={() => onChange({ layout: key })}>{label}</Button>)}
    </div>
    <div className="relative max-[800px]:col-span-2">
      <Search {...ICON} aria-hidden="true" className="pointer-events-none absolute top-2 left-2.5 text-subtle" />
      <Input type="search" name="board-search" className="board-search w-56 pl-8 max-[800px]:w-full" aria-label="Search tickets by title, agent or project" aria-keyshortcuts="/"
        placeholder="Search tickets" maxLength={128} autoComplete="off" value={view.search}
        onChange={(event) => onChange({ search: event.target.value })} onKeyDown={searchKey} />
    </div>
    <Select aria-label="Project" name="board-project" className={FILTER} value={view.project}
      onValueChange={(project) => onChange({ project })}
      options={[{ value: '', label: 'All projects' }, ...choices.projects]} />
    <Select aria-label="Agent" name="board-agent" className={FILTER} value={view.agent}
      onValueChange={(agent) => onChange({ agent })}
      options={[{ value: '', label: 'All agents' }, ...choices.agents]} />
    <Select aria-label="Status" name="board-status" className={FILTER} value={view.status}
      onValueChange={(status) => onChange({ status: status as BrowseView['status'] })}
      options={[{ value: '', label: 'All statuses' }, ...STATUS_CHOICES]} />
    {view.layout === 'list' ? <Select aria-label="Group by" name="board-group" className={FILTER} value={view.group}
      onValueChange={(group) => onChange({ group: group as GroupBy })}
      options={(Object.keys(GROUP_NAMES) as GroupBy[]).map((key) => ({ value: key, label: key === 'none' ? GROUP_NAMES[key] : `Group by ${GROUP_NAMES[key].toLowerCase()}` }))} /> : null}
    <p className="board-count m-0 flex items-center gap-2 text-small text-muted max-[800px]:col-span-2" role="status">
      {filtered ? `${shown} of ${total} match` : `${total} ${total === 1 ? 'ticket' : 'tickets'}`}
      {filtered ? <Button variant="ghost" size="sm" icon={<X {...ICON} />} className="board-clear" onClick={onClear}>Clear filters</Button> : null}
    </p>
    <IconButton label="Keyboard shortcuts" aria-keyshortcuts="?" className="board-help ml-auto max-[800px]:col-span-2 max-[800px]:ml-0 max-[800px]:justify-self-start" icon={<Keyboard {...ICON} />} onClick={onHelp} />
  </div>;
}
