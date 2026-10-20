// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What counts as the Floor telling a person to use a terminal. The renderer count and the
// browser journey both read this list, so a command the page can show is found the same way in
// the source and on the screen.

/** The first word after `dudes` that makes a line a command. Kept equal to the CLI's by a test. */
export const DUDES_COMMANDS = [
  'agents', 'ask', 'board', 'closing', 'config', 'doctor', 'floor', 'goal', 'halt', 'hire', 'login', 'look',
  'mail', 'mcp', 'memory', 'open', 'pause', 'ping', 'project', 'quickstart', 'recovery', 'rename', 'resume',
  'retire', 'start', 'status', 'stop', 'tail', 'team', 'ticket', 'uninstall', 'upgrade', 'why', 'workspace',
  'worktree',
];

const SECOND_WORDS = ['add', 'remove', 'reload', 'list', 'check', 'set', 'unset', 'open', 'append', 'inspect', 'submit', 'receipt'];

/** Each pattern finds one kind of terminal command in text. */
export const TERMINAL_PATTERNS = [
  { name: 'dudes command', pattern: new RegExp(`\\bdudes (?:${DUDES_COMMANDS.join('|')})\\b`) },
  { name: 'package manager', pattern: /\b(?:npm|npx|brew|pip|apt|apt-get|sudo)\s+(?:install|i|add|-g)\b/ },
  { name: 'shell prompt', pattern: /(?:^|\n)[ \t]*\$[ \t]+\S/ },
];

/** Source markers for a block made to be copied into a terminal. */
export const COPY_BLOCK_MARKERS = ['<Command', 'guide-command'];

/** Every terminal command found in `text`, as `{ kind, text }` with the line it starts. */
export function findTerminalCommands(text) {
  const found = [];
  for (const { name, pattern } of TERMINAL_PATTERNS) {
    const all = new RegExp(pattern.source, `${pattern.flags}g`);
    for (const match of text.matchAll(all)) {
      found.push({ kind: name, text: text.slice(match.index).split('\n').find((line) => line.trim() !== '').trim().slice(0, 70) });
    }
  }
  return found;
}

/** The distinct `dudes ...` commands named in `text`, such as `dudes config set`. */
export function dudesCommandsIn(text) {
  const second = SECOND_WORDS.join('|');
  const pattern = new RegExp(`\\bdudes (?:${DUDES_COMMANDS.join('|')})(?: (?:${second})\\b)?`, 'g');
  return [...text.matchAll(pattern)].map((match) => match[0]);
}

/** Whether a source line is a comment, which the page never shows. */
export function isCommentLine(line) {
  return /^\s*(?:\/\/|\/\*|\*)/.test(line);
}

/** Whether a source line can show a terminal command or a block to copy one. */
export function showsTerminalCommand(line) {
  return !isCommentLine(line)
    && (findTerminalCommands(line).length > 0 || COPY_BLOCK_MARKERS.some((marker) => line.includes(marker)));
}

/**
 * What a rendered screen shows that a person would run in a terminal. Text first, then any block
 * built to be copied: a copy-this-command row.
 */
export function terminalCommandsOnScreen({ text, codeBlocks }) {
  const found = findTerminalCommands(text);
  for (const block of codeBlocks) found.push({ kind: 'copy-this-command block', text: block.trim().slice(0, 80) });
  return found;
}
