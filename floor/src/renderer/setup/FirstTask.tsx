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

function Install({ command }: { readonly command: string }) {
  return <p className="first-task-install"><code>{command}</code>
    <button type="button" onClick={() => copyToClipboard(command)}>Copy</button>
    <button type="button" onClick={() => refreshFloorSnapshot()}>Recheck</button></p>;
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
  return <section className="first-task" aria-label="Next step" data-first-task={offer.kind}
    onKeyDown={(event) => {
      if (offer.kind !== 'task' || current.mode !== 'open' || !closesPanel(event)) return;
      event.preventDefault();
      event.stopPropagation();
      setPrompt({ freeIds, mode: 'collapsed' });
      openButton.current?.focus();
    }}>
    {offer.kind === 'task' ? <>
      <div className="first-task-line">
        <button type="button" className="first-task-open" ref={openButton} aria-expanded={current.mode === 'open'}
          onClick={() => setPrompt({ freeIds, mode: current.mode === 'open' ? 'collapsed' : 'open' })}>
          <span className="first-task-free">{offer.agents.length === 1
            ? `${offer.agents[0]!.name} is free` : `${offer.agents.length} agents are free`}</span>
          {' · Give a task'}</button>
        <button type="button" className="first-task-close"
          aria-label={current.mode === 'open' ? 'Close task form' : 'Hide task suggestion'}
          onClick={() => setPrompt({ freeIds, mode: current.mode === 'open' ? 'collapsed' : 'hidden' })}>×</button>
      </div>
      {current.mode === 'open' ? <FirstTaskForm floor={floor} agents={offer.agents} /> : null}
    </> : null}
    {offer.kind === 'install' ? <>
      <h2>Find {offer.engine === null ? 'the app' : appName(offer.engine)} for {offer.agent}</h2>
      <p>Dudes could not find {offer.engine === null ? 'the app' : appName(offer.engine)} for {offer.agent} on the
        daemon&apos;s PATH.{offer.engine !== null && INSTALL_COMMANDS[offer.engine] !== undefined
          ? ' If it is not installed, run this in a terminal, then press Recheck.' : ''}</p>
      <p>If it is already installed, check its location in <a href="#/setup">Setup</a>. The daemon may need a restart to find it.</p>
      {offer.engine !== null && INSTALL_COMMANDS[offer.engine] !== undefined
        ? <Install command={INSTALL_COMMANDS[offer.engine]!} /> : <p><button type="button"
          onClick={() => refreshFloorSnapshot()}>Recheck</button></p>}
    </> : null}
    {offer.kind === 'hire' ? <>
      <h2>Hire your first agent</h2>
      {offer.why === 'ready' ? <>
        <p>An agent is a coding assistant that works in one of your projects. Hire one, give it a task, and review
          what it did.</p>
        <p><button type="button" className="first-task-start" onClick={(event) => onHire(event.currentTarget)}>
          Hire an agent</button></p></> : null}
      {offer.why === 'no_app' ? <>
        <p>Dudes could not find Claude Code or Codex on the daemon&apos;s PATH. If neither is installed, install one, then press Recheck.</p>
        <p>If one is already installed, check <a href="#/setup">Setup</a>. The daemon may need a restart to find it.</p>
        <Install command={INSTALL_COMMANDS.claude!} /></> : null}
      {offer.why === 'no_project' ? <>
        {projectAddOn(floor) ? <>
          <p>Agents work in a project folder, and none is set up yet. Add the folder of your project.</p>
          <AddProject floor={floor} /></> : <>
          <p>Agents work in a project folder, and none is set up yet. Turn on Add and remove projects under
            Floor switches in <a href="#/settings">Settings</a>, then add your project folder.</p></>}</> : null}
      {offer.why === 'off' ? <p>Hiring from this page is turned off. Turn on Hire agents under Floor switches in{' '}
        <a href="#/settings">Settings</a>.</p> : null}
    </> : null}
  </section>;
}
