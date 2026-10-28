// SPDX-License-Identifier: MIT OR Apache-2.0

export type CopyOutcome = 'copied' | 'select';

export interface ClipboardPort {
  readonly writeText?: (text: string) => Promise<void>;
}

/**
 * Put `text` on the clipboard. The call must run inside the click that asked for it, so it starts before anything
 * awaits. A browser that has no clipboard or refuses the write gets `select`: the caller shows the text selected so
 * the operator can copy it by hand.
 */
export async function copyText(clipboard: ClipboardPort | null | undefined, text: string): Promise<CopyOutcome> {
  if (clipboard == null || typeof clipboard.writeText !== 'function') return 'select';
  try {
    await clipboard.writeText(text);
    return 'copied';
  } catch {
    return 'select';
  }
}
