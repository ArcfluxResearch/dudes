// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef, useState } from 'react';
import type { CSSProperties } from 'react';

import { lookKey } from './appearance.ts';
import type { Accessory, Look } from './appearance.ts';
import { lazyChunk } from '../transport.ts';

type PortraitStyle = 'figure' | 'badge';

/** Which portrait the Floor draws. A browser that cannot draw the figure gets the badge. */
const PORTRAIT_STYLE = 'figure' as PortraitStyle;

/**
 * The first character a reader sees in the name, upper-cased. A browser
 * without `Intl.Segmenter` gets the first code point, so there a combining
 * accent is dropped.
 */
export function initialOf(name: unknown): string {
  if (typeof name !== 'string') return '';
  const trimmed = name.trim();
  if (typeof Intl.Segmenter !== 'function') return Array.from(trimmed)[0]?.toLocaleUpperCase() ?? '';
  for (const { segment } of new Intl.Segmenter(undefined, { granularity: 'grapheme' }).segment(trimmed)) {
    return segment.toLocaleUpperCase();
  }
  return '';
}

/**
 * An agent's portrait: its office figure from the front, or its initial on its
 * Crew colour. Screen readers skip it: the drawer, the archive and the look
 * pickers each name the agent in text beside it.
 */
export function Portrait({ look, accessory = null, name, className }: {
  readonly look: Look;
  readonly accessory?: Accessory | null;
  readonly name: unknown;
  readonly className: string;
}) {
  const canvas = useRef<HTMLCanvasElement>(null);
  const key = lookKey(look, accessory);
  const [failedKey, setFailedKey] = useState<string | null>(null);
  const figure = PORTRAIT_STYLE === 'figure' && failedKey !== key;
  useEffect(() => {
    if (!figure) return;
    let current = true;
    lazyChunk(() => import('./figurePortrait.ts')).then(({ paintFigurePortrait }) => {
      if (current && canvas.current !== null && !paintFigurePortrait(canvas.current, look, accessory)) setFailedKey(key);
    }).catch(() => {
      if (current) setFailedKey(key);
    });
    return () => { current = false; };
  }, [figure, key]);
  return figure
    ? <canvas ref={canvas} className={`${className} portrait-figure`} aria-hidden="true" data-portrait="figure"
      data-skin={look.skin} data-hair={look.hair} data-shirt={look.shirt} />
    : <PortraitBadge look={look} name={name} className={className} />;
}

export function PortraitBadge({ look, name, className }: {
  readonly look: Look;
  readonly name: unknown;
  readonly className: string;
}) {
  return <span className={`${className} portrait-badge`} aria-hidden="true" data-portrait="badge"
    data-skin={look.skin} data-hair={look.hair} data-shirt={look.shirt}
    style={{ '--agent': look.shirt } as CSSProperties}>{initialOf(name)}</span>;
}
