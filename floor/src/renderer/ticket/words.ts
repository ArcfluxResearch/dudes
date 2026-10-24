// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The words a ticket is shown in. The daemon sends codes and counts, never a sentence, so the page
// keeps its own table of page guidance. The contract test checks coverage of the daemon codes.

import type { CardRow, TicketReason, TicketTurnEnd } from '../board.ts';

export type RefusalCode = 'stale_card' | 'not_a_ticket' | 'ticket_card' | 'wrong_state' | 'project_unavailable'
  | 'done_when_changed' | 'agent_unavailable' | 'agent_no_project' | 'queue_full' | 'limit_reached' | 'busy'
  | 'instruction_comparison_pending' | 'instruction_review_required';

export const REFUSAL_CODES: readonly RefusalCode[] = Object.freeze([
  'stale_card', 'not_a_ticket', 'ticket_card', 'wrong_state', 'project_unavailable', 'done_when_changed',
  'agent_unavailable', 'agent_no_project', 'queue_full', 'limit_reached', 'busy',
  'instruction_comparison_pending', 'instruction_review_required',
]);

const REFUSALS: Readonly<Record<Exclude<RefusalCode, 'wrong_state'>, string>> = Object.freeze({
  stale_card: "This ticket changed while you were looking. Showing the latest.",
  not_a_ticket: "That card is not a ticket.",
  ticket_card: "That card is a ticket. Open it on the Board to make changes.",
  project_unavailable: "That project is not set up any more.",
  done_when_changed: "The Done when check for this project changed. Choose it again.",
  agent_unavailable: "That agent is not available for this project.",
  agent_no_project: "That agent has no recorded project. Hire a new agent into this project from the Floor.",
  queue_full: "That agent already has 8 tickets waiting.",
  limit_reached: "Too many tickets for now. Remove some, or wait.",
  busy: "Another ticket request is still running. Try again in a moment.",
  instruction_comparison_pending: "Instruction comparison is still pending. Read the ticket again when it finishes.",
  instruction_review_required: "Read the current instruction review before acknowledging it.",
});

/** Text with the characters that reorder what is shown taken out, for anything a person or an agent wrote. */
export function readable(text: string): string {
  return text.replace(/[\u202A-\u202E\u2066-\u2069]/g, '');
}

/** What the operator calls a ticket in this status. */
export function stateName(status: CardRow['status']): string {
  switch (status) {
    case 'todo':
    case 'claimed': return 'To do';
    case 'doing': return 'Working';
    case 'review': return 'Ready for you';
    case 'done': return 'Done';
    case 'blocked': return 'Needs you';
  }
}

/** What a card without a ticket says of itself where no column fits: the state as recorded, and whose word it is. */
export function otherStateName(status: CardRow['status']): string {
  switch (status) {
    case 'review': return 'Waiting on a review';
    case 'blocked': return 'Blocked';
    case 'done': return 'Marked done by an agent, not checked';
    default: return stateName(status);
  }
}

export function refusalSentence(code: RefusalCode, status: CardRow['status'] | null): string {
  if (code === 'wrong_state') return `You can't do that to a ticket that is ${status === null ? 'in another state' : stateName(status)}.`;
  return REFUSALS[code];
}

const REASONS: Readonly<Record<TicketReason, string>> = Object.freeze({
  recovery_required: "This work was interrupted. Recovery keeps the same goal and ticket round. Review recovery to see what was recorded and choose how to continue. Earlier effects may repeat.",
  project_unavailable: "This ticket’s project is no longer set up. Add the project in Settings, then create a new ticket for it. Keep anything you need from this ticket before removing it.",
  check_cannot_run: "The completion check could not run. Its program may be missing or unsafe to run directly. Review the check in the project setup, then try again. If the check changed, remove this ticket and create it again.",
  ceiling_required: "This account bills per use and needs a spending limit. Review the project limits in Settings. Ask the person who set up the project to add a limit before trying again.",
  reserve_missing: "A spending reserve is missing. Set a per-turn reserve under Spending limits in Settings, then try again.",
  goal_changed: "The agent's goal was changed outside the board. Give to agent to try again, or Stop.",
  limit_goal: "This ticket reached its spending limit. Give to agent to try again, or Stop.",
  slowed: "The agent kept failing and was slowed down. Look at the agent, then Give to agent to try again, or Stop.",
  no_turns: "The agent used all its turns on this ticket. Give to agent to try again, or Stop.",
  held_launch: "The agent's engine would not start. Fix the cause, then resume the agent.",
  held_failing: "The agent's turns kept failing at once. Fix the cause, then resume the agent.",
  held_unsure: "Dudes could not confirm how the agent's last turn ended. Resume the agent, or Stop.",
  shared_instructions_changed: "Shared project instructions changed during this work. Review the change, then Accept or Ask for changes.",
  shared_instructions_unknown: "Dudes could not determine whether shared project instructions changed during this work. Review the work, then Accept or Ask for changes.",
  worktree_missing: "The agent's project copy is missing. Restore the folder and resume the agent, or Stop and give the ticket to another agent.",
});

/** The sentence that names the next step for a ticket in Needs you. */
export function reasonSentence(reason: TicketReason | null): string {
  return reason === null ? 'This ticket needs you.' : REASONS[reason];
}

/** Why a ticket that was given to an agent has not started. */
export type Wait = 'not_given' | 'agent_busy' | 'ticket' | 'project_busy' | 'agent_paused' | 'agent_attached'
  | 'closing' | 'stopping' | 'agent_limit' | 'floor_limit';

export const WAITS: readonly Wait[] = Object.freeze([
  'not_given', 'agent_busy', 'ticket', 'project_busy', 'agent_paused', 'agent_attached', 'closing', 'stopping',
  'agent_limit', 'floor_limit',
]);

/** The sentence for a wait. `agent` is the holder's name and `title` the ticket that holds the project. */
export function waitSentence(wait: Wait, agent: string, title: string): string {
  switch (wait) {
    case 'not_given': return "Not given to an agent yet. Choose Give to agent.";
    case 'agent_busy': return `Waiting for ${agent} to finish its current work.`;
    case 'ticket': return `Waiting for ${title} to be finished, accepted or sent back.`;
    case 'project_busy': return "Waiting for the project to be free.";
    case 'agent_paused': return `Waiting for ${agent} to be resumed.`;
    case 'agent_attached': return `Waiting for ${agent}'s keyboard session to end.`;
    case 'closing': return "Dudes is shutting down.";
    case 'stopping': return "Stopping: the agent is finishing its current step.";
    case 'agent_limit': return "The agent reached its daily limit. It carries on by itself when the limit resets.";
    case 'floor_limit': return "The Floor reached its daily limit. It carries on by itself when the limit resets.";
  }
}

/** "next in line", "2nd in line", "3rd in line". */
export function placeInLine(position: number): string {
  if (position <= 1) return 'next in line';
  const tens = position % 100;
  const suffix = tens >= 11 && tens <= 13 ? 'th' : ({ 1: 'st', 2: 'nd', 3: 'rd' } as Record<number, string>)[position % 10] ?? 'th';
  return `${position}${suffix} in line`;
}

/** The sentence shown with Give to agent when no agent is hired. */
export const NO_AGENT_YET = "No agent yet. Hire one, then give this ticket to it.";

export function noAgentInProjectSentence(project: string): string {
  return `No agent works in ${project} yet. Hire one into it, then give this ticket to it.`;
}

export function agentWithoutProjectSentence(agent: string): string {
  return `${agent} was hired before projects were recorded, so it can't take tickets.`;
}

/** Shown when an answer to a create or an action never arrived. */
export const COULD_NOT_CONFIRM = "We could not confirm that saved. Look at the board before you try again.";

/** Shown when a ticket's detail could not be read. A read changes nothing, so there is nothing to confirm. */
export const COULD_NOT_READ = "Dudes could not read this ticket. Choose Read again.";

/** Shown under the summary: whose words they are. */
export const AGENT_SAYS = "The agent says";
export const NO_SUMMARY = "The agent left no summary";
/** Shown under a summary whose reading is not confirmed current. */
export const FROM_LAST_READING = "This is from the last reading of the ticket. It may have changed since.";

const SIGNAL_NAMES: Readonly<Record<number, string>> = Object.freeze({
  1: 'SIGHUP', 2: 'SIGINT', 3: 'SIGQUIT', 4: 'SIGILL', 5: 'SIGTRAP', 6: 'SIGABRT',
  8: 'SIGFPE', 9: 'SIGKILL', 11: 'SIGSEGV', 13: 'SIGPIPE', 14: 'SIGALRM', 15: 'SIGTERM',
});

export function engineStopped(ended: TicketTurnEnd | undefined): string | null {
  if (ended === undefined) return null;
  const { code, signal } = ended.exit;
  const cause = signal !== null ? SIGNAL_NAMES[signal] ?? `signal ${signal}`
    : code !== null && code !== 0 ? `exit ${code}` : null;
  if (cause === null) return null;
  const time = new Date(ended.at).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', hour12: false });
  return `The engine stopped (${cause}) at ${time}`;
}
