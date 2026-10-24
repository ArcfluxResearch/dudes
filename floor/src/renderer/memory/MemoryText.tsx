// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Memory text drawn inert: React text children only, never markup, Markdown or a link. A
// character that draws nothing or changes how its neighbours draw is shown as a marker instead,
// one marker per run, each code point written `\u{…}` in lowercase hex.

import type { ReactNode } from 'react';

/**
 * Controls other than LF and TAB, format characters, default-ignorable, private-use and
 * unassigned code points, and the two Unicode line separators: the characters that hide text,
 * reorder it or vanish.
 */
const HIDDEN_RUN = /(?:(?![\n\t])[\p{Cc}\p{Cf}\p{Default_Ignorable_Code_Point}\p{Co}\p{Cn}\u2028\u2029])+/gu;

function escaped(run: string): string {
  let out = '';
  for (const character of run) out += `\\u{${(character.codePointAt(0) ?? 0).toString(16)}}`;
  return out;
}

export type HiddenPiece = string | { readonly marker: string };

const PICTOGRAPH = /^[\p{Extended_Pictographic}\p{Emoji_Modifier}]$/u;
const ZWJ = 0x200d;
const VARIATION_16 = 0xfe0f;
const KEYCAP = 0x20e3;
const BLACK_FLAG = 0x1f3f4;

function codePointBefore(text: string, index: number): number | null {
  if (index <= 0) return null;
  const low = text.charCodeAt(index - 1);
  if (low >= 0xdc00 && low <= 0xdfff && index >= 2) {
    const high = text.charCodeAt(index - 2);
    if (high >= 0xd800 && high <= 0xdbff) return text.codePointAt(index - 2) ?? null;
  }
  return low;
}

const isPictograph = (cp: number | null) => cp !== null && PICTOGRAPH.test(String.fromCodePoint(cp));
const isKeycapBase = (cp: number | null) => cp !== null && ((cp >= 0x30 && cp <= 0x39) || cp === 0x23 || cp === 0x2a);

/**
 * The hidden runs of a text as markers. With `emoji`, a zero-width joiner or variation selector
 * between emoji and a flag's tag sequence stay unmarked so emoji draw whole; everything else
 * hidden is still marked. Entities are never decoded.
 */
export function splitHidden(text: string, emoji = false): readonly HiddenPiece[] {
  const pieces: HiddenPiece[] = [];
  let at = 0;
  for (const run of text.matchAll(HIDDEN_RUN)) {
    if (run.index > at) pieces.push(text.slice(at, run.index));
    at = run.index + run[0].length;
    if (!emoji) {
      pieces.push({ marker: escaped(run[0]) });
      continue;
    }
    const cps = Array.from(run[0], (c) => c.codePointAt(0) ?? 0);
    const after = text.codePointAt(at) ?? null;
    let emojiBefore = isPictograph(codePointBefore(text, run.index));
    const before = codePointBefore(text, run.index);
    let keycapBase = isKeycapBase(before);
    let i = 0;
    let hidden = '';
    const flush = () => {
      if (hidden !== '') pieces.push({ marker: escaped(hidden) });
      hidden = '';
    };
    while (i < cps.length) {
      const cp = cps[i] ?? 0;
      const next = i + 1 < cps.length ? (cps[i + 1] ?? 0) : after;
      let keep = false;
      if (cp === VARIATION_16) keep = emojiBefore || (keycapBase && next === KEYCAP);
      else if (cp === ZWJ) keep = emojiBefore && i + 1 === cps.length && isPictograph(after);
      else if (cp >= 0xe0020 && cp <= 0xe007e && i === 0 && before === BLACK_FLAG) {
        let j = i;
        while (j < cps.length && (cps[j] ?? 0) >= 0xe0020 && (cps[j] ?? 0) <= 0xe007e) j += 1;
        if (cps[j] === 0xe007f) {
          flush();
          pieces.push(String.fromCodePoint(...cps.slice(i, j + 1)));
          i = j + 1;
          emojiBefore = false;
          keycapBase = false;
          continue;
        }
      }
      if (keep) {
        flush();
        pieces.push(String.fromCodePoint(cp));
        if (cp === ZWJ) emojiBefore = false;
      } else {
        hidden += String.fromCodePoint(cp);
        emojiBefore = false;
      }
      keycapBase = false;
      i += 1;
    }
    flush();
  }
  if (at < text.length) pieces.push(text.slice(at));
  return pieces;
}

export function MemoryText({ text }: { readonly text: string }) {
  const parts: ReactNode[] = splitHidden(text).map((piece, index) => typeof piece === 'string' ? piece
    : <span key={index} className="memory-marker">{piece.marker}</span>);
  return <div className="memory-text" dir="ltr">{parts}</div>;
}
