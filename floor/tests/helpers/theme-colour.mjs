// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';

export function themeTokens(css, theme) {
  const light = /:root\s*\{([^}]+)\}/.exec(css)?.[1] ?? '';
  const dark = /:root:has\(\.shell\[data-theme='dark'\]\)\s*\{([^}]+)\}/.exec(css)?.[1] ?? '';
  assert.ok(light && dark, 'both theme token sets exist');
  return Object.fromEntries([...`${light}${theme === 'dark' ? dark : ''}`
    .matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})/gi)].map((match) => [match[1], match[2]]));
}

export function themeColour(css, className, theme) {
  const rule = new RegExp(`\\.${className}\\s*\\{[^}]*?color:\\s*(#[0-9a-f]{6}|var\\(--([a-z-]+)\\))`, 'i').exec(css);
  assert.ok(rule, `.${className} sets a colour`);
  const colour = rule[2] === undefined ? rule[1] : themeTokens(css, theme)[rule[2]];
  assert.match(colour, /^#[0-9a-f]{6}$/i, `.${className} resolves its ${theme} colour`);
  return colour;
}
