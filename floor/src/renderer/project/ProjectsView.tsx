// SPDX-License-Identifier: MIT OR Apache-2.0

import { Plus, X } from 'lucide-react';
import { useEffect, useMemo, useRef, useState, useSyncExternalStore } from 'react';

import { Button, Card, Dialog, ICON, IconButton, Notice, cn } from '../../ui/index.ts';
import type { FloorState } from '../floor/store.ts';
import { Code, Fact, Facts, HINT, Tag } from '../PageParts.tsx';
import { ReadNotice, snapshotReadState } from '../ReadError.tsx';
import type { NeedState } from '../needs.ts';
import { restoreFocusAfterDialog } from '../focus.ts';
import { getPlaced, placeTickets, subscribeToPlaced } from '../ticket/placed.ts';
import { ticketActionsOpen } from '../ticket/TicketBoard.tsx';
import { resetAdd, resetCreate } from './add.ts';
import { AddProject, projectAddOn } from './AddProject.tsx';
import { TICKET_PLACES, projectRowsOf } from './projects.ts';
import type { ProjectAgent, ProjectRow } from './projects.ts';

const TONE: Readonly<Record<NeedState, 'ok' | 'warn' | 'neutral' | 'accent'>> = Object.freeze({
  working: 'ok', 'needs-you': 'warn', ready: 'accent', waiting: 'neutral', done: 'neutral', idle: 'neutral',
});

function AgentTag({ agent }: { readonly agent: ProjectAgent }) {
  return <li className="flex min-w-0 items-center gap-1.5 text-base">
    <span className="min-w-0 font-medium text-ink [overflow-wrap:anywhere]">{agent.name}</span>
    <Tag tone={TONE[agent.state]}>{agent.word}</Tag>
  </li>;
}

function Agents({ row }: { readonly row: ProjectRow }) {
  return row.agents.length === 0 ? <>No agents hired into this project yet.</>
    : <ul aria-label={`Agents in ${row.label}`} className="m-0 flex list-none flex-wrap gap-x-4 gap-y-1.5 p-0">
      {row.agents.map((agent) => <AgentTag key={agent.id} agent={agent} />)}
    </ul>;
}

function Tickets({ row, onOpenBoard }: { readonly row: ProjectRow; readonly onOpenBoard: (project: string) => void }) {
  return <ul aria-label={`Tickets in ${row.label}`} className="m-0 flex list-none flex-wrap gap-2 p-0">
    {TICKET_PLACES.map(({ key, title }) => {
      const count = row.tickets[key];
      return <li key={key}>
        <Button size="sm" data-ticket-place={key} title={`Open the board on ${row.label}`}
          className={cn('gap-1.5', count === 0 && 'text-muted')} onClick={() => onOpenBoard(row.id)}>
          <strong className="font-semibold">{count}</strong>{title}
        </Button>
      </li>;
    })}
  </ul>;
}

function Trust({ row }: { readonly row: ProjectRow }) {
  if (row.trusted === null) return <>Not reported.</>;
  return row.trusted ? <>Trusted. Claude agents can work in their own copy of it.</>
    : <>Not trusted, so agents work in the project folder itself.</>;
}

function Limits({ row }: { readonly row: ProjectRow }) {
  if (row.limits === null) return <>Not reported.</>;
  if (row.limits.length === 0) return <>No cap.</>;
  return <ul className="m-0 grid list-none gap-0.5 p-0">{row.limits.map((line) => <li key={line}>{line}</li>)}</ul>;
}

function ProjectCard({ row, onOpenBoard }: { readonly row: ProjectRow; readonly onOpenBoard: (project: string) => void }) {
  const headingId = `project-${row.id}`;
  return <li><Card role="region" aria-labelledby={headingId} data-project={row.id} className="grid gap-3 p-4">
    <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-2">
      <div className="grid min-w-0 gap-0.5">
        <h3 id={headingId} className="m-0 text-title font-semibold text-ink [overflow-wrap:anywhere]">{row.label}</h3>
        <Code className="w-fit bg-transparent px-0 text-muted">{row.id}</Code>
      </div>
      <Button data-project-board={row.id} aria-label={`Open board for ${row.label}`}
        onClick={() => onOpenBoard(row.id)}>Open board</Button>
    </div>
    <Facts>
      <Fact label="Agents"><Agents row={row} /></Fact>
      <Fact label="Tickets"><Tickets row={row} onOpenBoard={onOpenBoard} /></Fact>
      <Fact label="Done when">{row.check ?? 'Not reported.'}</Fact>
      <Fact label="Own-copy hires"><Trust row={row} /></Fact>
      <Fact label="Budget"><Limits row={row} /></Fact>
    </Facts>
  </Card></li>;
}

function AddProjectDialog({ floor, onClose }: { readonly floor: FloorState; readonly onClose: () => void }) {
  const dialog = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const node = dialog.current;
    resetAdd();
    resetCreate();
    node?.showModal();
    return () => { node?.close(); };
  }, []);
  return <Dialog ref={dialog} size="md" aria-labelledby="add-project-title" data-add-project-dialog="true"
    className="project-dialog"
    onCancel={(event) => { event.preventDefault(); onClose(); }}
    onKeyDown={(event) => { if (event.key === 'Escape') event.stopPropagation(); }}>
    <div className="grid gap-4 p-6">
      <div className="flex items-start justify-between gap-2">
        <h3 id="add-project-title" className="m-0 text-title font-semibold text-ink">Add a project</h3>
        <IconButton label="Close Add project" icon={<X {...ICON} />} onClick={onClose} />
      </div>
      <AddProject floor={floor} />
      <div className="flex justify-end"><Button onClick={onClose}>Done</Button></div>
    </div>
  </Dialog>;
}

/** Every enrolled project with what Dudes knows of it, and the one place to add another. */
export function ProjectsView({ floor, onOpenBoard }: {
  readonly floor: FloorState;
  /** Open the Board filtered to one project. */
  readonly onOpenBoard: (project: string) => void;
}) {
  const placed = useSyncExternalStore(subscribeToPlaced, getPlaced, getPlaced);
  const [adding, setAdding] = useState(false);
  const opener = useRef<HTMLElement | null>(null);
  const reported = floor.settings !== null || floor.onboarding !== null || floor.checks !== null;
  const state = snapshotReadState(floor, reported);
  const rows = useMemo(() => projectRowsOf(floor, placed), [floor.settings, floor.onboarding, floor.checks, floor.agents, floor.cards, floor.stepLabels, placed]);
  const unplaced = floor.cards.filter((card) => card.ticket !== null && card.claimed_by === null && !placed.has(card.id)).map((card) => card.id);
  const open = ticketActionsOpen(floor);
  useEffect(() => {
    if (open && unplaced.length > 0) void placeTickets(unplaced);
  }, [open, unplaced.join(',')]);
  function closeAdding(): void {
    setAdding(false);
    restoreFocusAfterDialog(opener.current, document.getElementById('add-project-open'));
  }
  return <div data-ui="projects" className="grid gap-4 font-sans text-ink">
    <div className="flex flex-wrap items-center justify-between gap-x-4 gap-y-2">
      <p className={cn(HINT, 'min-w-0 flex-1')}>Each project is a git folder your agents work in. Tickets from every project share one Board.</p>
      {projectAddOn(floor)
        ? <Button variant="primary" id="add-project-open" icon={<Plus {...ICON} />} aria-haspopup="dialog" aria-expanded={adding}
          onClick={(event) => { opener.current = event.currentTarget; setAdding(true); }}>Add project</Button>
        : null}
    </div>
    {floor.settings !== null && !projectAddOn(floor)
      ? <Notice tone="warn">Adding a project from this page is off. Turn on Add and remove projects under Floor switches in Settings.</Notice> : null}
    {floor.stale && state === 'ready' ? <p role="status" className={HINT}>Connection interrupted. These projects may be out of date.</p> : null}
    <ReadNotice state={state} label="Projects" message="Dudes did not report its projects. Retry to read them again." />
    {state !== 'ready' ? null : rows.length === 0
      ? <Card role="status" data-projects-empty="true" className="grid gap-1 p-6">
        <p className="m-0 text-title font-semibold text-ink">No projects yet</p>
        <p className={HINT}>Choose Add project to enrol a git folder, then hire an agent into it.</p>
      </Card>
      : <ul aria-label="Projects" className="m-0 grid list-none gap-3 p-0">{rows.map((row) => <ProjectCard key={row.id} row={row} onOpenBoard={onOpenBoard} />)}</ul>}
    {adding ? <AddProjectDialog floor={floor} onClose={closeAdding} /> : null}
  </div>;
}
