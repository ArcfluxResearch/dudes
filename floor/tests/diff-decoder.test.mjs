// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import test from 'node:test';
import { decodeDiffResponse } from '../src/renderer/diff/diff.ts';
import { diffBinding, diffContent, diffEntry, diffFile, diffPage } from './helpers/diff-fixtures.mjs';

const decode = (value) => decodeDiffResponse(JSON.stringify(value));

test('the public decoder retains each exact bounded response and freezes its entries', () => {
  for (const response of [diffPage(), diffPage({ mode: 'page' }), diffContent(), { mode: 'refused', reason: 'stale_ticket' }]) {
    assert.deepEqual(decode(response), response);
    assert.equal(Object.isFrozen(decode(response)), true);
  }
  assert.equal(Object.isFrozen(decode(diffPage()).entries[0]), true);
  assert.deepEqual(decode(diffContent({ files: [diffFile({ old: '', current: '<script>bad()</script>\nhttps://example.invalid/\n/tmp/file\n' })] })).files[0].current,
    '<script>bad()</script>\nhttps://example.invalid/\n/tmp/file\n');
});

test('unknown and missing fields fail closed at every public response boundary', () => {
  const missing = (object, key) => Object.fromEntries(Object.entries(object).filter(([name]) => name !== key));
  for (const response of [diffPage(), diffContent(), { mode: 'refused', reason: 'stale_ticket' }]) {
    for (const key of Object.keys(response)) assert.equal(decode(missing(response, key)), null, key);
    assert.equal(decode({ ...response, cwd: '/tmp/private' }), null);
  }
  for (const binding of [{ ...diffBinding(), author: 'x' }, missing(diffBinding(), 'check_lsn')]) {
    assert.equal(decode(diffPage({ binding })), null);
  }
  assert.equal(decode(diffPage({ entries: [diffEntry({ extra: true })] })), null);
  assert.equal(decode(diffPage({ entries: [diffEntry({ availability: { state: 'available', reason: 'binary' } })] })), null);
  assert.equal(decode(diffContent({ files: [diffFile({ extra: true })] })), null);
});

test('whole sides and lines accept their exact UTF-8 boundaries and reject every excess', () => {
  const old = `${'a'.repeat(4096)}\n`.repeat(3) + 'b'.repeat(4093);
  assert.equal(Buffer.byteLength(old), 16_384);
  assert.equal(decode(diffContent({ files: [diffFile({ old, current: old })] })).files[0].old, old);
  for (const text of [old + 'x', 'a'.repeat(4097), '\0', '\u001b[31mred', '\u202eevil', '\u200d', '\ud800']) {
    assert.equal(decode(diffContent({ files: [diffFile({ old: text })] })), null, JSON.stringify(text.slice(0, 20)));
  }
  assert.equal(decode(diffContent({ files: [diffFile({ old: 'a\tb\r\nc\rd\n' })] })).files[0].old, 'a\tb\r\nc\rd\n');
});

test('paths, opaque ids, counts and repeated response members are strict', () => {
  for (const path of ['', '/absolute', '../outside', 'a/../outside', 'a//b', 'a/./b', 'a/.GiT/b', 'a\\b', 'a\u202eb', 'x'.repeat(201)]) {
    assert.equal(decode(diffPage({ entries: [diffEntry({ path })] })), null, path);
  }
  for (const fields of [{ entries: Array(65).fill(diffEntry()) }, { untracked: -1 }, { omitted: 1.5 },
    { freshness: 'passed' }, { manifest_id: '../x' }, { next_cursor: 'bad cursor' }]) {
    assert.equal(decode(diffPage(fields)), null);
  }
  assert.equal(decode(diffPage({ entries: [diffEntry(), diffEntry()] })), null);
  assert.equal(decode(diffContent({ files: Array(9).fill(diffFile()) })), null);
  assert.equal(decode(diffContent({ files: [diffFile(), diffFile()] })), null);
  assert.equal(decode(diffPage({ entries: [diffEntry({ entry_id: null })] })), null);
  assert.equal(decode({ mode: 'refused', reason: 'invented' }), null);
  assert.equal(decodeDiffResponse('not json'), null);
  assert.equal(decodeDiffResponse(`${JSON.stringify(diffPage())}${' '.repeat(65_536)}`), null);
  assert.equal(decodeDiffResponse(`${JSON.stringify(diffContent())}${' '.repeat(131_072)}`), null);
});

test('page, file and serialized byte caps admit their exact boundary', () => {
  const entries = Array.from({ length: 64 }, (_, index) => diffEntry({ entry_id: index.toString(16).padStart(32, '0'), path: `src/file-${index}.rs` }));
  assert.equal(decode(diffPage({ entries })).entries.length, 64);
  const files = entries.slice(0, 8).map((entry) => diffFile({ entry_id: entry.entry_id, path: entry.path }));
  assert.equal(decode(diffContent({ files })).files.length, 8);
  const pageBody = JSON.stringify(diffPage());
  const contentBody = JSON.stringify(diffContent());
  assert.deepEqual(decodeDiffResponse(pageBody.padEnd(65_536, ' ')), diffPage());
  assert.equal(decodeDiffResponse(pageBody.padEnd(65_537, ' ')), null);
  assert.deepEqual(decodeDiffResponse(contentBody.padEnd(131_072, ' ')), diffContent());
  assert.equal(decodeDiffResponse(contentBody.padEnd(131_073, ' ')), null);
  const escaped = `${'"'.repeat(4096)}\n`.repeat(3);
  assert.equal(decode(diffContent({ files: [diffFile({ old: escaped, current: escaped })] })).files[0].old, escaped);
  assert.equal(decode(diffContent({ files: files.map((file) => ({ ...file, old: escaped, current: escaped })) })), null);
});

test('every finite refusal and whole-pair omission retains its exact reason', () => {
  for (const reason of ['baseline_unavailable', 'stale_ticket', 'stale_manifest', 'expired', 'invalid_request',
    'disabled', 'busy', 'session_limit', 'unsupported_platform', 'worktree_mismatch', 'unsupported_storage',
    'root_mismatch', 'timed_out', 'git_unavailable', 'unreadable']) {
    assert.deepEqual(decode({ mode: 'refused', reason }), { mode: 'refused', reason });
  }
  for (const reason of ['binary', 'unsupported_text', 'sensitive_path', 'sensitive_content', 'oversize_file',
    'oversize_line', 'unsupported_kind', 'raced', 'unreadable', 'response_limit']) {
    const entries = [diffEntry({ entry_id: null, availability: { state: 'unavailable', reason } })];
    assert.deepEqual(decode(diffPage({ entries })).entries, entries);
    const omitted = [{ entry_id: 'e'.repeat(32), reason }];
    assert.deepEqual(decode(diffContent({ files: [], omitted })).omitted, omitted);
  }
});

test('check positions preserve all canonical u64 decimal digits without number rounding', () => {
  for (const check_lsn of ['9007199254740993', '18446744073709551615']) {
    assert.equal(decode(diffPage({ binding: diffBinding({ check_lsn }) })).binding.check_lsn, check_lsn);
  }
  assert.equal(decode(diffPage({ binding: diffBinding({ check_lsn: null }), freshness: 'unknown' })).binding.check_lsn, null);
  for (const check_lsn of [42, '01', '-1', '1.5', '18446744073709551616']) {
    assert.equal(decode(diffPage({ binding: diffBinding({ check_lsn }) })), null);
  }
});

test('a missing check cannot carry either claimed sample comparison', () => {
  for (const freshness of ['matches_review_sample', 'changed_after_check']) {
    assert.equal(decode(diffPage({ freshness, binding: diffBinding({ check_lsn: null }) })), null);
    assert.equal(decode(diffContent({ freshness, binding: diffBinding({ check_lsn: null }) })), null);
  }
});
