// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Markup with the styling taken out, so a test can read the structure and the hook classes a screen keeps: the icons, the
 * utility classes and the owned components' `data-ui` marks are dropped, and a class attribute keeps only the names a test
 * or another module looks up. A utility changes whenever the design does; the hooks do not.
 */
const HOOK = /^(?:chat|step|memory|git|md|tok|completion)-/;

export function hooks(html) {
  return html
    .replace(/<svg\b[\s\S]*?<\/svg>/g, '')
    .replace(/ data-(?:ui|part|variant|size|loading)="[^"]*"/g, '')
    .replace(/ class="([^"]*)"/g, (_, names) => {
      const kept = names.split(/\s+/).filter((name) => HOOK.test(name));
      return kept.length === 0 ? '' : ` class="${kept.join(' ')}"`;
    });
}
