// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';

export function themeTokens(css, theme) {
  const light = /:root\s*\{([^}]+)\}/.exec(css)?.[1] ?? '';
  const dark = /:root:has\(\.shell\[data-theme='dark'\]\)\s*\{([^}]+)\}/.exec(css)?.[1] ?? '';
  assert.ok(light && dark, 'both theme token sets exist');
  return Object.fromEntries([...`${light}${theme === 'dark' ? dark : ''}`
    .matchAll(/--([a-z-]+):\s*(#[0-9a-f]{6})/gi)].map((match) => [match[1], match[2]]));
}

/** The Tailwind entry file's tokens for `theme`: `bg` is the page the lines of the office are read against. */
export async function entryTokens(theme) {
  const { readFile } = await import('node:fs/promises');
  const { resolve } = await import('node:path');
  return themeTokens(await readFile(resolve(import.meta.dirname, '../../src/tailwind.css'), 'utf8'), theme);
}

export function themeColour(css, className, theme) {
  const rule = new RegExp(`\\.${className}\\s*\\{[^}]*?color:\\s*(#[0-9a-f]{6}|var\\(--([a-z-]+)\\))`, 'i').exec(css);
  assert.ok(rule, `.${className} sets a colour`);
  const colour = rule[2] === undefined ? rule[1] : themeTokens(css, theme)[rule[2]];
  assert.match(colour, /^#[0-9a-f]{6}$/i, `.${className} resolves its ${theme} colour`);
  return colour;
}

const TEXT_TOKEN = { danger: 'danger', muted: 'text-muted', ink: 'text', subtle: 'text-subtle', accent: 'accent', warn: 'warn', ok: 'ok' };

/** The class string a component source names with `pattern`'s one capture group. */
export async function classesIn(file, pattern) {
  const { readFile } = await import('node:fs/promises');
  const { resolve } = await import('node:path');
  const text = await readFile(resolve(import.meta.dirname, '../../src/renderer', file), 'utf8');
  const found = pattern.exec(text)?.[1];
  assert.ok(found, `${file} has the class string ${pattern}`);
  return found;
}

/** The colour a class string's text colour utility resolves to in `theme`, from the Tailwind entry file's tokens. */
export async function utilityColour(classes, theme) {
  const { readFile } = await import('node:fs/promises');
  const { resolve } = await import('node:path');
  const name = /(?:^|\s)text-(danger|muted|ink|subtle|accent|warn|ok)(?:\s|$)/.exec(classes)?.[1];
  assert.ok(name, `${classes} sets a text colour from a token`);
  const entry = await readFile(resolve(import.meta.dirname, '../../src/tailwind.css'), 'utf8');
  const colour = themeTokens(entry, theme)[TEXT_TOKEN[name]];
  assert.match(colour, /^#[0-9a-f]{6}$/i, `text-${name} resolves its ${theme} colour`);
  return colour;
}
