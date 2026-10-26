// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Where the failure line is rendered, and whether it can be read there.
//
// Firing control for this file: move the `office-failure` paragraph back inside
// the `<div className="office">` in OfficeCanvas.tsx — "the failure line is a
// sibling of the office box" goes red, naming the depth it was found at, and
// the contrast case goes red with 2.54:1 against the box's own background.
//
// This reads the component's source rather than a rendered tree on purpose. The
// floor ships no DOM test harness and may add no dependency to get one, and the
// `failure` state is set from inside an effect, so a server render would report
// the branch as absent whichever side of the box it was written on. What matters
// here is structural — which element the paragraph is a child of —
// and that is a fact about this file, checked here by counting nesting rather
// than by matching a shape of text.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';
import { classesIn, themeTokens, utilityColour } from './helpers/theme-colour.mjs';
import { legacyStyles } from './helpers/legacy-css.mjs';

const source = resolve(import.meta.dirname, '../src/renderer/OfficeCanvas.tsx');

const failureColour = async (theme) => utilityColour(await classesIn('ReadError.tsx', /className=\{cn\(className, '([^']*)'\)\}/), theme);

/** The office box's own background, which is what the line sits on. */
const BOX = '#241f1a';
/**
 * The floor's readability bar, stated as a threshold and not a colour: a mark
 * reads at >= 4.5:1 against any surface it can sit on, measured rather than
 * chosen by eye. Every line on this page is held to it.
 */
const READABLE = 4.5;

function contrast(foreground, background) {
  const channel = (value) => {
    const c = value / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  const luminance = (hex) => {
    const n = Number.parseInt(hex.slice(1), 16);
    return 0.2126 * channel((n >> 16) & 255)
      + 0.7152 * channel((n >> 8) & 255)
      + 0.0722 * channel(n & 255);
  };
  const [lighter, darker] = [luminance(foreground), luminance(background)].sort((a, b) => b - a);
  return (lighter + 0.05) / (darker + 0.05);
}

/**
 * How many `div` elements are open at `index`, counting from the start of the
 * file. Detail content may add nested divs; a paragraph inside the office is
 * still at positive depth, while the sibling failure line is at depth 0.
 */
function divDepthAt(text, index) {
  let depth = 0;
  for (const tag of text.slice(0, index).matchAll(/<(\/?)div\b[^>]*?(\/?)>/g)) {
    if (tag[1] === '/') depth -= 1;
    else if (tag[2] !== '/') depth += 1;
  }
  return depth;
}

test('the office box is identified with detail in the drawer beside it', async () => {
  const text = await readFile(source, 'utf8');
  const opens = [...text.matchAll(/<div\b[^>]*?>/g)];
  const office = opens.filter(([tag]) => /className="office /.test(tag));
  assert.equal(office.length, 1, 'exactly one div is the office box');
  assert.equal(opens.filter(([tag]) => /id="selected-agent-detail"/.test(tag)).length, 0,
    'selected detail is no longer inside the office');
  const drawer = await readFile(resolve(import.meta.dirname, '../src/renderer/AgentDrawer.tsx'), 'utf8');
  assert.match(drawer, /<div className=\{[^}]*\} id="selected-agent-detail"/);
  const control = await readFile(resolve(import.meta.dirname, '../src/renderer/Control.tsx'), 'utf8');
  assert.match(control, /<dialog className="ticket-dialog" id="selected-card-detail"/);
  const planted = '<div className="office"><p className="office-failure">failed</p></div>';
  assert.equal(divDepthAt(planted, planted.indexOf('office-failure')), 1,
    'the depth scan catches a failure line planted inside the office');
});

test('the failure line is a sibling of the office box, not a child of it', async () => {
  const text = await readFile(source, 'utf8');
  const at = text.indexOf('className="office-failure"');
  assert.notEqual(at, -1, 'the failure paragraph is still rendered');
  assert.ok(at > text.indexOf('<div className="office '), 'the failure line follows the office box');
  const depth = divDepthAt(text, at);
  assert.equal(
    depth,
    0,
    `the failure line is ${depth} div deep, so it draws over the studio instead of on the page`,
  );
});

test('the failure line reads against the page at the brief threshold', async () => {
  const css = await legacyStyles();
  for (const theme of ['light', 'dark']) {
    const colour = await failureColour(theme);
    const onPage = contrast(colour, themeTokens(css, theme).surface);
    assert.ok(onPage >= READABLE,
      `${colour} reads at ${onPage.toFixed(2)}:1 on the ${theme} page, under the ${READABLE}:1 bar`);
  }
  const colour = await failureColour('light');
  // And the reason it had to move: inside the box it never cleared the bar.
  assert.ok(
    contrast(colour, BOX) < READABLE,
    `${colour} clears ${READABLE}:1 against the box too, so this line's placement is not what fixed it`,
  );
});

test('the failure line is held to the same bar as the unconfirmed line', async () => {
  const css = await legacyStyles();
  for (const theme of ['light', 'dark']) {
    const page = themeTokens(css, theme).surface;
    const failure = contrast(await failureColour(theme), page);
    const stale = contrast(await utilityColour('text-warn', theme), page);
    assert.ok(Math.min(failure, stale) >= READABLE,
      `the ${theme} lines read at ${failure.toFixed(2)}:1 and ${stale.toFixed(2)}:1, under the ${READABLE}:1 bar`);
  }
});
