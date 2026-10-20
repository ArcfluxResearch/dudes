// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A person adds a "project". "Workspace" is the old name for the same thing, and seeing both
// left people unsure whether they were two features. Cargo's `--workspace` flag and Codex's
// `workspace-write` sandbox are other things and keep their names.

/** Each use of the old word in `text`, with a little context around it. */
export function oldProjectWords(text) {
  return [...text.matchAll(/\bworkspaces?\b/gi)]
    .filter(({ index }) => !text.slice(Math.max(0, index - 2), index).endsWith('--'))
    .filter(({ 0: word, index }) => !/^[-_]?write/i.test(text.slice(index + word.length)))
    .map(({ index }) => text.slice(Math.max(0, index - 30), index + 40).replace(/\s+/g, ' ').trim());
}
