// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The class merger: a later utility replaces an earlier one of the same kind, so a base style and a variant on one
// element never both reach it. Without it the winner is whichever rule Tailwind happens to emit last, and a secondary
// button once lost its whole border that way.

import assert from 'node:assert/strict';
import test from 'node:test';

import { cn } from '../src/ui/cn.ts';

test('a later utility replaces an earlier one of the same kind and keeps the rest', () => {
  assert.equal(cn('border-transparent bg-accent', 'border-line-strong'), 'bg-accent border-line-strong');
  assert.equal(cn('px-2 py-1', 'px-3'), 'py-1 px-3');
  assert.equal(cn('hover:bg-accent-hover', 'hover:bg-surface-2', 'bg-surface'), 'hover:bg-surface-2 bg-surface');
});

test('a width, a colour and a style of one border are different kinds and all stay', () => {
  assert.equal(cn('border border-line-strong', 'border-b-2 border-accent'), 'border border-b-2 border-accent');
  assert.equal(cn('border border-line', 'border-transparent'), 'border border-transparent');
});

test('the type scale, the radii and the shadows of the Floor are known to the merger', () => {
  assert.equal(cn('text-base text-ink', 'text-small'), 'text-ink text-small');
  assert.equal(cn('text-small text-muted', 'text-danger'), 'text-small text-danger');
  assert.equal(cn('text-body', 'text-caption'), 'text-caption');
  assert.equal(cn('rounded-md', 'rounded-pill'), 'rounded-pill');
  assert.equal(cn('shadow-card', 'shadow-pop'), 'shadow-pop');
  assert.equal(cn('font-medium', 'font-semibold'), 'font-semibold');
});

test('false, null and undefined parts are dropped, and a variant of one utility does not replace another variant', () => {
  assert.equal(cn('a-x', false, null, undefined, 'b-y'), 'a-x b-y');
  assert.equal(cn('bg-surface hover:bg-surface-2', 'focus-visible:bg-accent-soft'), 'bg-surface hover:bg-surface-2 focus-visible:bg-accent-soft');
  assert.equal(cn('size-8', 'h-7'), 'size-8 h-7');
});
