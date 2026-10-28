// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The questions the operator has answered, as the stream carries them: the question's own words, already
// scrubbed by the daemon, and the positions of the question and of its answer. The answer text is not in
// the stream and is not kept here.

const AGENT_ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;
const POSITION = /^[1-9][0-9]{0,19}$/;
/** The daemon keeps 64 across every agent. */
const MAX_KEPT = 64;

export interface AnsweredQuestion {
  readonly agent: string;
  /** The position of the question's record, as decimal text. */
  readonly questionLsn: string;
  readonly question: string;
}

export const NO_QUESTIONS: readonly AnsweredQuestion[] = Object.freeze([]);

/** The rows this page can read. A malformed row is left out, and a list longer than the daemon keeps is ignored. */
export function decodeQuestionHistory(value: unknown): readonly AnsweredQuestion[] {
  if (!Array.isArray(value) || value.length > MAX_KEPT) return NO_QUESTIONS;
  const rows: AnsweredQuestion[] = [];
  for (const raw of value) {
    if (typeof raw !== 'object' || raw === null) continue;
    const { agent_id: agent, question_lsn: lsn, question } = raw as Record<string, unknown>;
    if (typeof agent !== 'string' || !AGENT_ID.test(agent) || typeof lsn !== 'string' || !POSITION.test(lsn)
      || typeof question !== 'string') continue;
    rows.push(Object.freeze({ agent, questionLsn: lsn, question }));
  }
  return Object.freeze(rows);
}
