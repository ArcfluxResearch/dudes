// SPDX-License-Identifier: MIT OR Apache-2.0

import { useRef, useState } from 'react';
import type { FloorState } from '../floor/store.ts';
import { closesPanel } from '../focus.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { INSTALL_COMMANDS } from './daemon.ts';
import { copyToClipboard } from './SetupView.tsx';
import { appName, firstTaskOffer, freeTaskAgents } from './nextStep.ts';
import { FirstTaskForm } from '../ticket/TicketBoard.tsx';
import { viewConfirmed } from '../ticket/model.ts';
import { AddProject, projectAddOn } from '../project/AddProject.tsx';
import { X } from 'lucide-react';
import { Button, ICON, IconButton, cn } from '../../ui/index.ts';

const CARD = 'first-task box-border grid gap-3 overflow-auto rounded-lg border border-line bg-surface p-4 font-sans text-body leading-reading text-ink shadow-pop [&_p]:m-0';
const DOCKED = 'min-[801px]:absolute min-[801px]:inset-x-0 min-[801px]:bottom-auto min-[801px]:top-3 min-[801px]:z-(--layer-panels) min-[801px]:mx-auto min-[801px]:max-h-[calc(100%-32px)] min-[801px]:w-[min(480px,calc(100%-32px))]';
const CENTRED = 'fixed inset-0 z-(--layer-panels) m-auto h-fit max-h-[calc(100dvh-32px)] w-[min(480px,calc(100vw-32px))] transform-none';

function Install({ command }: { readonly command: string }) {
  return <p className="first-task-install flex flex-wrap items-center gap-2">
    <code className="rounded-md border border-line bg-surface-2 px-2 py-1 font-mono text-small">{command}</code>
    <Button size="sm" onClick={() => copyToClipboard(command)}>Copy</Button>
    <Button size="sm" onClick={() => refreshFloorSnapshot()}>Recheck</Button></p>;
}

type TaskPrompt = {
  readonly freeIds: string;
  readonly mode: 'collapsed' | 'open' | 'hidden';
};

export function FirstTask({ floor, onHire }: {
  readonly floor: FloorState;
  readonly onHire: (opener: HTMLElement | null) => void;
}) {
  const freeIds = JSON.stringify(freeTaskAgents(floor).map((agent) => agent.id).sort());
  const [prompt, setPrompt] = useState<TaskPrompt>({ freeIds, mode: 'collapsed' });
  const openButton = useRef<HTMLButtonElement>(null);
  const current: TaskPrompt = !viewConfirmed(floor) || prompt.freeIds === freeIds
    ? prompt : { freeIds, mode: 'collapsed' };
  if (current !== prompt) setPrompt(current);
  const offer = firstTaskOffer(floor);
  if (offer === null || (offer.kind === 'task' && current.mode === 'hidden')) return null;
  return <section className={cn(CARD, offer.kind === 'hire' ? CENTRED : DOCKED)} aria-label="Next step" data-first-task={offer.kind}
    onKeyDown={(event) => {
      if (offer.kind !== 'task' || current.mode !== 'open' || !closesPanel(event)) return;
      event.preventDefault();
      event.stopPropagation();
      setPrompt({ freeIds, mode: 'collapsed' });
      openButton.current?.focus();
    }}>
    {offer.kind === 'task' ? <>
      <div className="first-task-line flex items-center gap-2">
        <Button variant="ghost" className="first-task-open min-w-0 flex-1 justify-start px-2" ref={openButton} aria-expanded={current.mode === 'open'}
          onClick={() => setPrompt({ freeIds, mode: current.mode === 'open' ? 'collapsed' : 'open' })}>
          <span className="first-task-free min-w-0 truncate text-ink">{offer.agents.length === 1
            ? `${offer.agents[0]!.name} is free` : `${offer.agents.length} agents are free`}</span>
          {' · Give a task'}</Button>
        <IconButton className="first-task-close min-h-0 min-w-0 flex-none" icon={<X {...ICON} />}
          label={current.mode === 'open' ? 'Close task form' : 'Hide task suggestion'}
          onClick={() => setPrompt({ freeIds, mode: current.mode === 'open' ? 'collapsed' : 'hidden' })} />
      </div>
      {current.mode === 'open' ? <FirstTaskForm floor={floor} agents={offer.agents} /> : null}
    </> : null}
    {offer.kind === 'install' ? <>
      <h2 className="m-0 text-title font-semibold">Find {offer.engine === null ? 'the engine' : appName(offer.engine)} for {offer.agent}</h2>
      <p>Dudes could not find {offer.engine === null ? 'the engine' : appName(offer.engine)} for {offer.agent}.{offer.engine !== null && INSTALL_COMMANDS[offer.engine] !== undefined
          ? ' If it is not installed, run this in a terminal, then press Recheck.' : ''}</p>
      <p>If it is already installed, check its location in <a className="text-accent underline underline-offset-2" href="#/setup">Setup</a>. Dudes may need a restart to find it.</p>
      {offer.engine !== null && INSTALL_COMMANDS[offer.engine] !== undefined
        ? <Install command={INSTALL_COMMANDS[offer.engine]!} /> : <p><Button size="sm"
          onClick={() => refreshFloorSnapshot()}>Recheck</Button></p>}
    </> : null}
    {offer.kind === 'hire' ? <>
      <h2 className="m-0 text-title font-semibold">Hire your first agent</h2>
      {offer.why === 'ready' ? <>
        <p>An agent is a coding assistant that works in one of your projects. Hire one, give it a task, and review
          what it did.</p>
        <p><Button variant="primary" className="first-task-start" onClick={(event) => onHire(event.currentTarget)}>
          Hire an agent</Button></p></> : null}
      {offer.why === 'no_app' ? <>
        <p>Dudes could not find Claude or Codex. If neither engine is installed, install one, then press Recheck.</p>
        <p>If one is already installed, check <a className="text-accent underline underline-offset-2" href="#/setup">Setup</a>. Dudes may need a restart to find it.</p>
        <Install command={INSTALL_COMMANDS.claude!} /></> : null}
      {offer.why === 'no_project' ? <>
        {projectAddOn(floor) ? <>
          <p>Agents work in a project folder, and none is set up yet. Add the folder of your project.</p>
          <AddProject floor={floor} /></> : <>
          <p>Agents work in a project folder, and none is set up yet. Turn on Add and remove projects under
            Floor switches in <a className="text-accent underline underline-offset-2" href="#/settings">Settings</a>, then add your project folder.</p></>}</> : null}
      {offer.why === 'off' ? <p>Hiring from this page is turned off. Turn on Hire agents under Floor switches in{' '}
        <a className="text-accent underline underline-offset-2" href="#/settings">Settings</a>.</p> : null}
    </> : null}
  </section>;
}
