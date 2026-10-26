// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Markup with the styling taken out, so a test reads the structure and the words: icons go, the owned components'
 * `data-ui`, `data-variant` and `data-size` hooks go, and a `class` keeps only the names the test lists, since a
 * utility class changes whenever the design does.
 */
export function bare(html, keep = []) {
  return html
    .replace(/<svg\b[\s\S]*?<\/svg>/g, '')
    .replace(/ data-(?:ui|variant|size|loading)="[^"]*"/g, '')
    .replace(/ class="([^"]*)"/g, (_, names) => {
      const kept = names.split(/\s+/).filter((name) => keep.includes(name));
      return kept.length === 0 ? '' : ` class="${kept.join(' ')}"`;
    });
}
