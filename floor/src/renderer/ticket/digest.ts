// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The steps an engine reported for a ticket's newest turn, as one sentence. Counts only, and
// labelled as what the engine reported: an agent can change files through a shell command, which
// no edit step records.

import type { StepDigest } from '../board.ts';

const KINDS: readonly (readonly [keyof StepDigest, string, string])[] = [
  ['command', 'command', 'commands'],
  ['read', 'file read', 'file reads'],
  ['edit', 'edit', 'edits'],
  ['write', 'file written', 'files written'],
  ['search', 'search', 'searches'],
  ['web', 'web lookup', 'web lookups'],
  ['plan', 'plan update', 'plan updates'],
  ['subagent', 'helper agent', 'helper agents'],
  ['mcp', 'tool call', 'tool calls'],
  ['other', 'other step', 'other steps'],
];

export const DIGEST_CAVEAT = 'Counted from the engine’s own step list. A change made through a shell command is not counted as an edit.';

export function digestSentence(digest: StepDigest): string | null {
  const parts = KINDS.filter(([key]) => digest[key] > 0)
    .map(([key, one, many]) => `${digest[key]} ${digest[key] === 1 ? one : many}`);
  if (parts.length === 0) return null;
  const files = digest.files_edited;
  const filesNote = digest.edit + digest.write === 0 ? '' : ` Edits and writes named ${files} ${files === 1 ? 'file' : 'files'}.`;
  return `The engine reported ${parts.join(', ')}.${filesNote}`;
}
