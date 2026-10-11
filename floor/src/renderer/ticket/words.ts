// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The words a ticket is shown in. The daemon sends codes and counts, never a sentence, so the page
// keeps a table of them. A test in the daemon's suite reads this file and fails when a sentence
// here differs from the one the daemon owns.

import type { CardRow, TicketReason } from '../board.ts';

export type RefusalCode = 'stale_card' | 'not_a_ticket' | 'ticket_card' | 'wrong_state' | 'project_unavailable'
  | 'done_when_changed' | 'agent_unavailable' | 'agent_no_project' | 'queue_full' | 'limit_reached' | 'busy';

export const REFUSAL_CODES: readonly RefusalCode[] = Object.freeze([
  'stale_card', 'not_a_ticket', 'ticket_card', 'wrong_state', 'project_unavailable', 'done_when_changed',
  'agent_unavailable', 'agent_no_project', 'queue_full', 'limit_reached', 'busy',
]);

const REFUSALS: Readonly<Record<Exclude<RefusalCode, 'wrong_state'>, string>> = Object.freeze({
  stale_card: "This ticket changed while you were looking. Showing the latest.",
  not_a_ticket: "That card is not a ticket.",
  ticket_card: "That card is a ticket. Use the ticket commands.",
  project_unavailable: "That project is not set up any more.",
  done_when_changed: "The Done when check for this project changed. Choose it again.",
  agent_unavailable: "That agent is not available for this project.",
  agent_no_project: "That agent was hired before projects were recorded. Hire a new agent in this project with `dudes hire`.",
  queue_full: "That agent already has 8 tickets waiting.",
  limit_reached: "Too many tickets for now. Remove some, or wait.",
  busy: "Another ticket request is still running. Try again in a moment.",
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

export function refusalSentence(code: RefusalCode, status: CardRow['status'] | null): string {
  if (code === 'wrong_state') return `You can't do that to a ticket that is ${status === null ? 'in another state' : stateName(status)}.`;
  return REFUSALS[code];
}

const REASONS: Readonly<Record<TicketReason, string>> = Object.freeze({
  project_unavailable: "This project is not set up any more. Enroll it again with `dudes workspace add`, then Give to agent, or Remove the ticket.",
  check_cannot_run: "The check's program was not found. Install it, then Give to agent. If the check itself is wrong, Remove this ticket and create it again.",
  ceiling_required: "Set a spending limit first, because your account bills per use. Add one to the project's enrollment (`dudes workspace add`) or the agent's hire, then Give to agent.",
  reserve_missing: "A per-turn spending reserve is missing. Run `dudes config set budget.per_turn_floor.usd <dollars>` (or `budget.per_turn_floor.tokens <count>`), then Give to agent.",
  goal_changed: "The agent's goal was changed outside the board. Give to agent to try again, or Stop.",
  limit_goal: "This ticket reached its spending limit. Give to agent to try again, or Stop.",
  slowed: "The agent kept failing and was slowed down. Look at the agent, then Give to agent to try again, or Stop.",
  no_turns: "The agent used all its turns on this ticket. Give to agent to try again, or Stop.",
  held_launch: "The agent's engine would not start. Fix the cause, then resume the agent in the Floor or with `dudes resume`.",
  held_failing: "The agent's turns kept failing at once. Fix the cause, then resume the agent in the Floor or with `dudes resume`.",
  held_unsure: "Dudes could not confirm how the agent's last turn ended. Resume the agent, or Stop.",
  worktree_missing: "The agent's worktree is missing. Restore it and resume the agent, or Stop and give the ticket to another agent.",
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

/** Shown under the summary: whose words they are. */
export const AGENT_SAYS = "The agent says";
export const NO_SUMMARY = "The agent left no summary";
