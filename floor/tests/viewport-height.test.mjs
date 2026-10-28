// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Every Floor view is exactly as tall as the window and the page itself never scrolls. A browser check of that
// lives in the release journey. These checks catch the two causes that broke it, without a browser.

import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';
import { relative, resolve } from 'node:path';
import test from 'node:test';

const floor = resolve(import.meta.dirname, '..');

async function sources(dir = resolve(floor, 'src')) {
  const found = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const path = resolve(dir, entry.name);
    if (entry.isDirectory()) found.push(...await sources(path));
    else if (/\.tsx?$/.test(entry.name)) found.push(path);
  }
  return found;
}

/**
 * Tailwind compiles `max-[N px]` to a width below N and `min-[N px]` to a width of at least N. A pair that names the
 * same width on both sides leaves that exact width with neither layout, so a window of exactly 800 pixels got the
 * desktop rules without the desktop layout. The phone layout is up to 800 pixels, so it is `max-[801px]`. The
 * breakpoints below each pair a `max` of one more than the last phone width with a `min` of the first wide width.
 */
const BREAKPOINTS = new Set(['641', '801', '1200']);

test('every width variant names a breakpoint that leaves no window width without a layout', async () => {
  const offenders = [];
  for (const path of await sources()) {
    const text = await readFile(path, 'utf8');
    for (const [token, width] of text.matchAll(/\b(?:min|max)-\[(\d+(?:\.\d+)?)px\]/g).map((match) => [match[0], match[1]])) {
      if (!BREAKPOINTS.has(width)) offenders.push(`${relative(floor, path)}: ${token}`);
    }
  }
  assert.deepEqual([...new Set(offenders)], []);
});

test('the shell is the window high and the stage scrolls on a phone instead of the page', async () => {
  const shell = await readFile(resolve(floor, 'src/shell.tsx'), 'utf8');
  const root = /<main ref=\{shellRef\}[^>]*?className="([^"]*)"/s.exec(shell)?.[1] ?? '';
  assert.match(root, /(?:^| )h-dvh(?: |$)/, 'the shell is exactly the window high in every layout');
  assert.doesNotMatch(root, /(?:^|[ :])(?:h-auto|h-screen|min-h-|max-h-)/, 'nothing lets the shell grow past the window');
  assert.match(shell, /!fullPanel && 'max-\[801px\]:overflow-y-auto'/, 'a phone stage taller than the window scrolls inside itself');
});
