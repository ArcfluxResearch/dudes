// SPDX-License-Identifier: MIT OR Apache-2.0

import { Columns3, List, Search, X } from 'lucide-react';
import type { KeyboardEvent, ReactNode } from 'react';

import { Button, ICON, Input, Select } from '../../ui/index.ts';
import { GROUP_NAMES, STATUS_CHOICES, isFiltered } from './browse.ts';
import type { BoardLayout, BrowseView, Choices, GroupBy } from './browse.ts';

const LAYOUTS: readonly { readonly key: BoardLayout; readonly label: string; readonly icon: ReactNode }[] = Object.freeze([
  { key: 'board', label: 'Board', icon: <Columns3 {...ICON} /> },
  { key: 'list', label: 'List', icon: <List {...ICON} /> },
]);

const FILTER = 'w-40 max-[800px]:w-full';

/** Layout, search, filters and grouping for the Board, and how many tickets they leave. */
export function BoardToolbar({ view, choices, total, shown, onChange, onClear }: {
  readonly view: BrowseView;
  readonly choices: Choices;
  readonly total: number;
  readonly shown: number;
  readonly onChange: (patch: Partial<BrowseView>) => void;
  readonly onClear: () => void;
}) {
  const filtered = isFiltered(view);
  function searchKey(event: KeyboardEvent<HTMLInputElement>): void {
    if (event.key !== 'Escape' || view.search === '') return;
    event.preventDefault();
    onChange({ search: '' });
  }
  return <div className="board-toolbar flex flex-wrap items-center gap-2 max-[800px]:flex-col max-[800px]:items-stretch" role="group" aria-label="Show tickets">
    <div role="group" aria-label="Layout" className="inline-flex gap-0.5 rounded-md bg-surface-2 p-0.5 max-[800px]:self-start">
      {LAYOUTS.map(({ key, label, icon }) => <Button key={key} variant="ghost" size="sm" icon={icon}
        className="board-layout aria-pressed:bg-surface aria-pressed:text-ink aria-pressed:shadow-card" data-layout={key}
        aria-pressed={view.layout === key} onClick={() => onChange({ layout: key })}>{label}</Button>)}
    </div>
    <div className="relative max-[800px]:w-full">
      <Search {...ICON} aria-hidden="true" className="pointer-events-none absolute top-2 left-2.5 text-subtle" />
      <Input type="search" name="board-search" className="board-search w-56 pl-8 max-[800px]:w-full" aria-label="Search tickets by title, agent or project"
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
    <p className="board-count m-0 flex items-center gap-2 text-small text-muted" role="status">
      {filtered ? `${shown} of ${total} match` : `${total} ${total === 1 ? 'ticket' : 'tickets'}`}
      {filtered ? <Button variant="ghost" size="sm" icon={<X {...ICON} />} className="board-clear" onClick={onClear}>Clear filters</Button> : null}
    </p>
  </div>;
}
