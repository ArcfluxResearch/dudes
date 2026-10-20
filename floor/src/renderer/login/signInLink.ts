// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The sign-in link an engine printed, read back from what the terminal drew.
//
// The page never builds or edits a link: it only recognises one the engine wrote. A link that was
// soft-wrapped across rows is joined back from the rows the grid marks as one line. A link the
// engine broke across rows itself, with a newline of its own, cannot be told from two lines of
// text, so it is not offered at all; the terminal still shows it.

/** One row of the drawn screen: its text without trailing spaces, and whether it continues the row above. */
export interface ScreenRow {
  readonly text: string;
  readonly wrapped: boolean;
}

/** Characters that make a link read differently from where it goes: controls, zero-width marks and direction overrides. */
const HIDDEN = '\\u0000-\\u001f\\u007f-\\u009f\\u200b-\\u200f\\u202a-\\u202e\\u2066-\\u2069\\ufeff';
const LINK = /https:\/\/[^\s"'<>\u0000-\u001f\u007f]+/g;

/** Whether `text` is a link the page will offer: https, with a plain host and no credentials in it. */
export function offerableLink(text: string): boolean {
  return /^https:\/\/[A-Za-z0-9](?:[A-Za-z0-9.-]*[A-Za-z0-9])?(?::\d{1,5})?(?:[/?#]|$)/.test(text)
    && !new RegExp(`[${HIDDEN}]`).test(text);
}

/** The last link printed on `rows`, exactly as printed, or `null` when none is certain. `cols` is the grid's width. */
export function printedLink(rows: readonly ScreenRow[], cols: number): string | null {
  let found: string | null = null;
  let at = 0;
  while (at < rows.length) {
    let text = rows[at]!.text;
    let last = at;
    while (rows[last + 1]?.wrapped === true) {
      last += 1;
      text += rows[last]!.text;
    }
    const next = rows[last + 1];
    const mayContinue = rows[last]!.text.length >= cols && next !== undefined && /^\S/.test(next.text);
    for (const match of text.matchAll(LINK)) {
      const endsLine = match.index + match[0].length === text.length;
      if (endsLine && mayContinue) found = null;
      else if (offerableLink(match[0])) found = match[0];
    }
    at = last + 1;
  }
  return found;
}

/** The link a terminal hyperlink sequence names, from the sequence's own text, or `null` when it names none. */
export function hyperlinkTarget(data: string): string | null {
  const target = data.slice(data.indexOf(';') + 1);
  return offerableLink(target) ? target : null;
}
