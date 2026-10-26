// SPDX-License-Identifier: MIT OR Apache-2.0

/** The classes a test or a script finds a part of the page by. Every other class is a utility that only styles. */
const HOOK = /^(?:ticket-|diff-|git-|recovery-|login-|card-|first-task|board-|notification-|receipts$|md$|data$|visually-hidden$)/;

/**
 * Server-rendered markup with the styling taken out: no utility classes, no `data-ui` bookkeeping, no icons, and a
 * disclosure's body unwrapped. What is left is the structure and wording a screen is read by, so a test about words
 * and order does not change when the design does.
 */
export function plainMarkup(html) {
  return html
    .replace(/<svg\b[^>]*>[\s\S]*?<\/svg>/g, '')
    .replace(/ data-(?:ui|variant|size|loading|part|state)="[^"]*"/g, '')
    .replace(/ class="([^"]*)"/g, (_, value) => {
      const kept = value.split(/\s+/).filter((token) => HOOK.test(token));
      return kept.length === 0 ? '' : ` class="${kept.join(' ')}"`;
    })
    .replace(/<\/summary><div>/g, '</summary>')
    .replace(/<\/div><\/details>/g, '</details>');
}
