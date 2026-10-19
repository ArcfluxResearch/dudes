// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The status badge over a figure's head: a white disc with one glyph in the
// state's colour. Drawn on a 96-pixel 2D surface the renderer turns into a
// texture; the surface is a parameter, so a test can record the strokes.

import type { Badge } from '../scene/stations.ts';

export const BADGE_SIZE = 96;

export const BADGE_COLOURS: Readonly<Record<Badge, string>> = Object.freeze({
  working: '#4b8fd6',
  checking: '#a77bd4',
  question: '#e0a12c',
  held: '#e0a12c',
  done: '#3e9a5c',
  attached: '#6b4fa0',
  exhausted: '#8d8f93',
  paused: '#8d8f93',
  halted: '#8d8f93',
  review: '#d9602b',
});

export type BadgeSurface = Pick<CanvasRenderingContext2D, 'beginPath' | 'arc' | 'fill' | 'stroke' | 'moveTo' | 'lineTo'
  | 'fillRect' | 'strokeRect' | 'fillText' | 'clearRect'> & {
  fillStyle: string | CanvasGradient | CanvasPattern;
  strokeStyle: string | CanvasGradient | CanvasPattern;
  lineWidth: number;
  lineCap: CanvasLineCap;
  lineJoin: CanvasLineJoin;
  font: string;
  textAlign: CanvasTextAlign;
  textBaseline: CanvasTextBaseline;
};

/**
 * A badge for a browser without a 2D offscreen canvas: a disc in the state's
 * colour with a white rim, as `size` by `size` RGBA pixels. It keeps the
 * colour and drops the glyph.
 */
export function discPixels(colour: string, size: number): Uint8Array {
  const rgb = [1, 3, 5].map((at) => Number.parseInt(colour.slice(at, at + 2), 16));
  const pixels = new Uint8Array(size * size * 4);
  const centre = (size - 1) / 2;
  const radius = size / 2 - 1;
  for (let y = 0; y < size; y += 1) {
    for (let x = 0; x < size; x += 1) {
      const distance = Math.hypot(x - centre, y - centre);
      if (distance > radius) continue;
      const rim = distance > radius - Math.max(1, size / 10);
      pixels.set(rim ? [255, 255, 255, 255] : [...rgb, 255], (y * size + x) * 4);
    }
  }
  return pixels;
}

function line(surface: BadgeSurface, points: readonly (readonly [number, number])[]): void {
  surface.beginPath();
  points.forEach(([x, y], index) => (index === 0 ? surface.moveTo(x, y) : surface.lineTo(x, y)));
  surface.stroke();
}

export function drawBadge(surface: BadgeSurface, badge: Badge): void {
  const colour = BADGE_COLOURS[badge];
  surface.clearRect(0, 0, BADGE_SIZE, BADGE_SIZE);
  surface.fillStyle = '#ffffff';
  surface.strokeStyle = 'rgba(59, 50, 41, 0.9)';
  surface.lineWidth = 5;
  surface.beginPath();
  surface.arc(48, 46, 34, 0, Math.PI * 2);
  surface.fill();
  surface.stroke();
  if (badge === 'question') {
    surface.beginPath();
    surface.moveTo(40, 76);
    surface.lineTo(48, 92);
    surface.lineTo(56, 76);
    surface.fill();
  }
  surface.strokeStyle = colour;
  surface.fillStyle = colour;
  surface.lineWidth = 7;
  surface.lineCap = 'round';
  surface.lineJoin = 'round';
  surface.font = 'bold 44px system-ui, sans-serif';
  surface.textAlign = 'center';
  surface.textBaseline = 'middle';
  switch (badge) {
    case 'working':
      [30, 46, 62].forEach((y, i) => line(surface, [[30, y], [66 - i * 10, y]]));
      return;
    case 'checking':
      surface.beginPath();
      surface.arc(43, 41, 13, 0, Math.PI * 2);
      surface.stroke();
      line(surface, [[53, 51], [64, 62]]);
      return;
    case 'held':
    case 'paused':
      surface.fillRect(35, 30, 9, 32);
      surface.fillRect(52, 30, 9, 32);
      return;
    case 'halted':
      surface.fillRect(33, 31, 30, 30);
      return;
    case 'exhausted':
      surface.lineWidth = 5;
      surface.strokeRect(28, 34, 36, 24);
      surface.fillRect(64, 41, 5, 10);
      surface.fillRect(32, 38, 7, 16);
      return;
    case 'done':
      line(surface, [[31, 47], [43, 59], [66, 34]]);
      return;
    case 'question':
      surface.fillText('?', 48, 48);
      return;
    case 'review':
      surface.fillText('!', 48, 49);
      return;
    case 'attached':
      surface.font = 'bold 34px ui-monospace, monospace';
      surface.fillText('>_', 48, 47);
      return;
  }
}
