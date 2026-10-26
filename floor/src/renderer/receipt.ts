// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The look of the receipts for the requests a page sends from the agent's panel (pause, stop, retire, answer, task
// change), and of the sentences inside them. One place, so the five receipts and the forms beside them read alike.

import { RING } from '../ui/index.ts';

/** The card a receipt is drawn in. */
export const RECEIPT = 'box-border grid gap-3 rounded-lg border border-line bg-surface p-4 font-sans text-base text-ink shadow-card [overflow-wrap:anywhere]';

/** A receipt's heading. It takes focus when the page hands it over, so it carries the ring. */
export const RECEIPT_TITLE = `m-0 flex flex-wrap items-baseline gap-x-2 rounded-sm font-sans text-title font-semibold text-ink ${RING}`;

/** The quiet word before a receipt's heading. */
export const KICKER = 'font-sans text-base font-medium text-muted';

export const FACTS = 'm-0 grid grid-cols-1 gap-1 text-small';
export const FACT = 'flex flex-wrap gap-x-2 border-0 p-0';
export const FACT_NAME = 'text-small font-normal normal-case tracking-normal text-muted';
export const FACT_VALUE = 'm-0 font-mono text-ink';

/** One quiet sentence. */
export const NOTE = 'm-0 text-small text-muted';

/** A row of buttons. */
export const ACTIONS = 'flex flex-wrap items-center gap-2';

const MESSAGE = 'm-0 rounded-md border px-3 py-2 text-base leading-normal text-ink';

/** A sentence a request earned: it went wrong, it worked, or it is still open. */
export const MESSAGE_CLASS = {
  error: `${MESSAGE} border-danger/40 bg-danger-soft`,
  success: `${MESSAGE} border-ok/40 bg-ok-soft`,
  pending: `${MESSAGE} border-line bg-surface-2`,
} as const;

/** The sentence a person asked to have repeated back: their own answer or task. */
export const QUOTE = 'm-0 rounded-sm border-0 border-l-2 border-solid border-accent bg-surface-2 px-3 py-2 text-base whitespace-pre-wrap [overflow-wrap:anywhere]';

/** A label above a text field, and the radio rows under a legend. */
export const LABEL = 'text-small font-medium text-ink';
export const CHOICE = `inline-flex cursor-pointer items-center gap-2 text-base text-ink`;
export const RADIO = `m-0 size-4 accent-accent ${RING}`;
