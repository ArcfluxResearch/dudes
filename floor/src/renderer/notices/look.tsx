// SPDX-License-Identifier: MIT OR Apache-2.0

import { CircleAlert, CircleCheck, Info, TriangleAlert } from 'lucide-react';

import { ICON, RING, cn } from '../../ui/index.ts';

/** A link in running text: the accent colour, underlined, with the one focus ring. */
export const LINK = cn('rounded-sm text-accent underline underline-offset-2 hover:text-accent-hover', RING);

export type NoticeTone = 'info' | 'warn' | 'danger' | 'ok';

const INK: Readonly<Record<NoticeTone, string>> = { info: 'text-info', warn: 'text-warn', danger: 'text-danger', ok: 'text-ok' };
const EDGE: Readonly<Record<NoticeTone, string>> = { info: 'border-line', warn: 'border-warn', danger: 'border-danger', ok: 'border-ok' };
const WASH: Readonly<Record<NoticeTone, string>> = { info: 'bg-surface', warn: 'bg-warn-soft', danger: 'bg-danger-soft', ok: 'bg-ok-soft' };

/** The notice stack's host, fixed over the bottom-left corner of the stage, and the banner row under the top bar. */
export const NOTICE_AREA = cn(
  'notice-area pointer-events-none fixed bottom-[max(16px,env(safe-area-inset-bottom,0px))] left-4 z-(--layer-notices) box-border flex',
  'max-h-[min(70dvh,448px)] w-[min(416px,calc(100vw-32px))] flex-col justify-end in-data-[focus=true]:left-auto in-data-[focus=true]:right-4 max-[800px]:left-2 max-[800px]:right-2 max-[800px]:w-auto',
);
export const NOTICE_BANNER = cn(
  'notice-banner relative z-(--layer-notices) box-border data-[host=fixed]:fixed data-[host=fixed]:inset-x-0 data-[host=fixed]:top-0',
  'data-[host=sticky]:sticky data-[host=sticky]:top-0 max-[800px]:data-[host=top]:fixed max-[800px]:data-[host=top]:inset-x-0 max-[800px]:data-[host=top]:top-0',
);

/** A floating notice: one card for every kind, its edge naming how serious it is. */
export function noticeCard(tone: NoticeTone = 'info', className?: string): string {
  return cn('pointer-events-auto m-0 box-border w-fit max-w-full rounded-lg border bg-surface p-3 font-sans text-base font-normal leading-normal text-ink shadow-pop [overflow-wrap:anywhere]',
    EDGE[tone], className);
}

/** A line of the banner under the top bar: full width, washed with the tone's colour. */
export function noticeLine(tone: NoticeTone = 'info', className?: string): string {
  return cn('m-0 box-border border-b px-4 py-2 font-sans text-base font-normal leading-normal text-ink [overflow-wrap:anywhere]', EDGE[tone], WASH[tone], className);
}

/** The icon that opens a notice's first line. It sits in the text, so the notice keeps its own element and wording. */
export function NoticeIcon({ tone = 'info' }: { readonly tone?: NoticeTone }) {
  const props = { ...ICON, 'aria-hidden': true, className: cn('-mt-0.5 mr-2 inline align-middle', INK[tone]) } as const;
  return tone === 'ok' ? <CircleCheck {...props} /> : tone === 'warn' ? <TriangleAlert {...props} />
    : tone === 'danger' ? <CircleAlert {...props} /> : <Info {...props} />;
}
