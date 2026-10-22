// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The first task of a hire. The daemon's hire carries no task, so once it answers `complete` the page
// makes the typed task a ticket in the hire's project, with the check the hire named, and gives it to
// the new agent through the same two requests the board's own form uses.
//
// A ticket that was not saved keeps the typed text in the draft and may be tried again. One that was
// saved but not given stays in To do, where the board's Give action finishes it. This module is apart
// from the hire's own state so that the hire does not depend on the ticket requests.

import { browserControlDeps } from '../control/control.ts';
import { titleOf } from '../setup/nextStep.ts';
import { createTicket, getTicketState } from '../ticket/ticket.ts';
import { checkHireStatus, getHireState, setFirstTask, submitHire, trimAsciiSpace } from './hire.ts';
import type { HireDeps, HireOffer } from './hire.ts';

/** Make the first task a ticket and give it to the hired agent. Only a complete hire has an agent. */
export async function makeFirstTask(deps: HireDeps): Promise<void> {
  const { attempt, outcome, task: before, draft } = getHireState();
  if (attempt === null || outcome?.kind !== 'complete' || before.phase === 'making' || before.phase === 'made') return;
  setFirstTask({ phase: 'making' });
  const text = trimAsciiSpace(draft.taskText);
  const title = titleOf(text);
  const created = await createTicket({
    title, note: text === title ? '' : text, project: attempt.payload.workspace_id,
    check: { id: attempt.payload.check_id, revision: attempt.payload.check_revision }, agent: outcome.agent,
  }, deps.tickets ?? browserControlDeps);
  if (getHireState().attempt !== attempt) return;
  const notice = getTicketState().notice;
  setFirstTask(created.saved && created.card !== null
    ? { phase: 'made', card: created.card, given: notice?.tone === 'done', text: notice?.text ?? 'The ticket was created.' }
    : { phase: 'not_made', text: notice?.text ?? 'Another ticket request is still running. Try again in a moment.' });
}

/** Hire, and when the daemon says the agent is hired, start its first task. */
export async function hireWithFirstTask(offer: HireOffer, deps: HireDeps): Promise<void> {
  await submitHire(offer, deps);
  await makeFirstTask(deps);
}

/** Ask the daemon again about the same attempt, and start the first task if the hire turns out complete. */
export async function checkHireStatusWithFirstTask(deps: HireDeps): Promise<void> {
  await checkHireStatus(deps);
  await makeFirstTask(deps);
}
