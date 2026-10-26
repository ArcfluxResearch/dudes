// SPDX-License-Identifier: MIT OR Apache-2.0

import { TriangleAlert } from 'lucide-react';
import { ICON, cn } from '../../ui/index.ts';
import { ASSISTANT_BUBBLE, SPEAKER } from './look.ts';

/** Controls, format characters and line or paragraph separators, other than a line break. */
const HIDDEN = /[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu;

/** The text with each hidden character drawn as its code point, and every other character as it is. */
export function visibleFailure(text: string): string {
  return text.replace(HIDDEN, (char) => {
    if (char === '\n' || char === '\t') return char;
    const code = char.codePointAt(0) ?? 0;
    return `[U+${code.toString(16).toUpperCase().padStart(4, '0')}]`;
  });
}

/** The engine's own words for a failed turn. Plain text, never Markdown, and never a reply. */
export function EngineSaid({ failure }: { readonly failure: string }) {
  return <div className={cn('chat-message chat-engine-said border-danger/40 bg-danger-soft', ASSISTANT_BUBBLE)} data-chat-failure="true">
    <p className={cn('chat-speaker flex items-center gap-1.5 text-danger', SPEAKER)}>
      <TriangleAlert {...ICON} aria-hidden="true" className="shrink-0" />The engine said</p>
    <pre className="chat-failure-text m-0 font-sans text-base leading-normal whitespace-pre-wrap text-ink [overflow-wrap:anywhere]">{visibleFailure(failure)}</pre>
  </div>;
}
