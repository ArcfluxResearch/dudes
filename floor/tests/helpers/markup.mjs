// SPDX-License-Identifier: MIT OR Apache-2.0

import { renderToStaticMarkup as render } from 'react-dom/server';

/**
 * A Tailwind utility: a variant or arbitrary value, a known keyword, or a known prefix. Every other class name is a hook
 * the markup keeps for tests, the focus handoff and the few rules that still select by name.
 */
const PREFIX = ['m', 'mx', 'my', 'mt', 'mb', 'ml', 'mr', 'p', 'px', 'py', 'pt', 'pb', 'pl', 'pr', 'gap', 'gap-x', 'gap-y', 'space-x',
  'space-y', 'w', 'h', 'min-w', 'max-w', 'min-h', 'max-h', 'size', 'text', 'font', 'leading', 'tracking', 'bg', 'border', 'rounded',
  'shadow', 'ring', 'outline', 'opacity', 'z', 'inset', 'grid-cols', 'grid-rows', 'col-span', 'row-span', 'flex', 'items', 'justify',
  'self', 'place', 'overflow', 'whitespace', 'cursor', 'transition', 'animate', 'accent', 'basis', 'grow', 'shrink', 'translate',
  'object', 'aspect', 'decoration', 'align', 'sr', 'box', 'divide'];
/** Prefixes that also begin a hook's name (`top-bar`), so only a number, a size word or an arbitrary value after them is a utility. */
const POSITION = /^-?(?:top|right|bottom|left)-(?:\d|px$|full$|auto$|\[|\()/;
const KEYWORD = new Set(['flex', 'grid', 'block', 'inline', 'inline-flex', 'inline-block', 'contents', 'hidden', 'relative', 'absolute',
  'fixed', 'sticky', 'truncate', 'uppercase', 'lowercase', 'normal-case', 'underline', 'no-underline', 'italic', 'shrink', 'grow', 'group',
  'resize-y', 'resize', 'select-none', 'select-text', 'list-none', 'list-disc', 'border', 'rounded', 'shadow', 'ring', 'outline',
  'transition', 'tabular-nums', 'invisible', 'visible']);
const UTILITY = new RegExp(`^-?(?:${PREFIX.join('|')})(?:-.*)?$`);
const isUtility = (name) => /[:[(/!]/.test(name) || KEYWORD.has(name) || UTILITY.test(name) || POSITION.test(name);

/** Components draw these for styling and tests of other kinds; a markup test about words and structure ignores them. */
const NOISE = / data-(?:ui|variant|size|loading|part)="(?!notice")[^"]*"/g;

function keep(tag, attributes) {
  let moved = '';
  const bare = attributes.replace(/\sclass="([^"]*)"/, (whole, names) => {
    const kept = names.split(/\s+/).filter((name) => !isUtility(name)).join(' ');
    if (kept !== '' && (tag === 'button' || tag === 'dialog')) moved = ` class="${kept}"`;
    return kept === '' || moved !== '' ? '' : ` class="${kept}"`;
  });
  if (moved === '') return bare;
  // A component puts its class last; the markup these tests read had it right after `type`, or first.
  const first = /^\s+type="button"(?:\s+role="[^"]*")?|^\s+[^\s=]+(?:="[^"]*")?/.exec(bare)?.[0] ?? '';
  return `${bare.slice(0, first.length)}${moved}${bare.slice(first.length)}`;
}

/** A class string's hooks, in order: what is left when its Tailwind utilities are taken out. */
export function hookClasses(className) {
  if (className === null || className === undefined) return null;
  return className.split(/\s+/).filter((name) => name !== '' && !isUtility(name)).join(' ');
}

/** `renderToStaticMarkup` with each element's utility classes and component-only attributes left out. */
export function renderToStaticMarkup(...args) {
  return render(...args).replace(NOISE, '').replace(/<([a-z][a-z0-9]*)((?:\s+[^\s>=]+(?:="[^"]*")?)*)(\s*\/?)>/g,
    (whole, tag, attributes, close) => `<${tag}${keep(tag, attributes)}${close}>`);
}

/** The `data-` marks the owned components draw for styling; a markup test about words and structure ignores them. */
const MARKS = { hook: 'ui|part|variant|size|loading', plain: 'ui|variant|size|loading|part|state', bare: 'ui|variant|size|loading' };

/**
 * Markup with the styling taken out: icons go, the components' `data-` marks named by `marks` go, and a class attribute
 * keeps only the names `keep` accepts (a RegExp or a list of names).
 */
function strip(html, { keep, marks }) {
  const accepts = Array.isArray(keep) ? (name) => keep.includes(name) : (name) => keep.test(name);
  return html
    .replace(/<svg\b[\s\S]*?<\/svg>/g, '')
    .replace(new RegExp(` data-(?:${marks})="[^"]*"`, 'g'), '')
    .replace(/ class="([^"]*)"/g, (_, names) => {
      const kept = names.split(/\s+/).filter(accepts);
      return kept.length === 0 ? '' : ` class="${kept.join(' ')}"`;
    });
}

const HOOK_NAMES = /^(?:chat|step|memory|git|md|tok|completion)-/;
const PLAIN_NAMES = /^(?:ticket-|diff-|git-|recovery-|login-|card-|first-task|board-|notification-|receipts$|md$|data$|visually-hidden$)/;

/** Keeps the structure and the hook classes a screen is read by: the chat, step, memory, git, markdown and token names. */
export const hooks = (html) => strip(html, { keep: HOOK_NAMES, marks: MARKS.hook });

/** Also unwraps a disclosure's body, so a test about words and order does not change when the design does. */
export const plainMarkup = (html) => strip(html, { keep: PLAIN_NAMES, marks: MARKS.plain })
  .replace(/<\/summary><div>/g, '</summary>')
  .replace(/<\/div><\/details>/g, '</details>');

/** Keeps only the class names the test lists. */
export const bare = (html, keep = []) => strip(html, { keep, marks: MARKS.bare });
