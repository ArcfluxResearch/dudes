// SPDX-License-Identifier: MIT OR Apache-2.0

import './engine-said.css';

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
  return <div className="chat-message chat-engine-said" data-chat-failure="true">
    <p className="chat-speaker">The engine said</p>
    <pre className="chat-failure-text">{visibleFailure(failure)}</pre>
  </div>;
}
