// SPDX-License-Identifier: MIT OR Apache-2.0

import type { FloorState } from '../floor/store.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { INSTALL_COMMANDS } from './daemon.ts';
import { copyToClipboard } from './SetupView.tsx';
import { appName, firstTaskOffer } from './nextStep.ts';
import { FirstTaskForm } from '../ticket/TicketBoard.tsx';
import { AddProject, projectAddOn } from '../project/AddProject.tsx';

function Install({ command }: { readonly command: string }) {
  return <p className="first-task-install"><code>{command}</code>
    <button type="button" onClick={() => copyToClipboard(command)}>Copy</button>
    <button type="button" onClick={() => refreshFloorSnapshot()}>Recheck</button></p>;
}

/** The one obvious next step on a Floor where nobody has work yet. */
export function FirstTask({ floor, onHire }: {
  readonly floor: FloorState;
  readonly onHire: (opener: HTMLElement | null) => void;
}) {
  const offer = firstTaskOffer(floor);
  if (offer === null) return null;
  return <section className="first-task" aria-label="Next step" data-first-task={offer.kind}>
    {offer.kind === 'task' ? <FirstTaskForm floor={floor} agents={offer.agents} /> : null}
    {offer.kind === 'install' ? <>
      <h2>Install {offer.engine === null ? 'the app' : appName(offer.engine)} for {offer.agent}</h2>
      <p>{offer.agent} needs {offer.engine === null ? 'its app' : appName(offer.engine)}, which is not installed on
        this computer yet.{offer.engine !== null && INSTALL_COMMANDS[offer.engine] !== undefined
          ? ' Run this in a terminal, then press Recheck.' : ''}</p>
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
        <p>Dudes needs Claude Code or Codex on this computer to run an agent. Install one, then press Recheck.</p>
        <Install command={INSTALL_COMMANDS.claude!} /></> : null}
      {offer.why === 'no_project' ? <>
        {projectAddOn(floor) ? <>
          <p>Agents work in a project folder, and none is set up yet. Add the folder of your project.</p>
          <AddProject floor={floor} /></> : <>
          <p>Agents work in a project folder, and none is set up yet. In your project&apos;s folder, run{' '}
            <code>dudes project add</code>, then press Recheck.</p>
          <p><button type="button" onClick={() => refreshFloorSnapshot()}>Recheck</button></p></>}</> : null}
      {offer.why === 'off' ? <p>Hiring from this page is turned off. Run <code>dudes hire</code> in a terminal to add
        an agent.</p> : null}
    </> : null}
  </section>;
}
