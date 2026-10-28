// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The size and line-ending rules for a ticket note. The daemon counts UTF-8 bytes and refuses a carriage return, while a
// text box's `maxLength` counts UTF-16 units, so the page counts bytes itself and sends line feeds only.

export const NOTE_LIMIT_BYTES = 8_192;

const encoder = new TextEncoder();

export function noteBytes(text: string): number {
  return encoder.encode(text).length;
}

/** Every line ending as a line feed: CRLF and a lone CR both. */
export function toLf(text: string): string {
  return text.replace(/\r\n?/g, '\n');
}

/** The sentence that refuses a note over the limit, or null when it fits. */
export function noteTooLong(text: string): string | null {
  const bytes = noteBytes(toLf(text));
  return bytes <= NOTE_LIMIT_BYTES
    ? null
    : `The note is ${bytes.toLocaleString('en-US')} bytes and the limit is ${NOTE_LIMIT_BYTES.toLocaleString('en-US')}. Shorten it before saving.`;
}

/**
 * What a save sends after an edit in the rich editor: the original string while the editor's Markdown still equals what
 * it wrote when it opened, so a note nobody changed is never rewritten, and the editor's Markdown once it differs.
 */
export function nextNote(original: string, opened: string, markdown: string): string {
  return markdown === opened ? original : markdown;
}
