// SPDX-License-Identifier: MIT OR Apache-2.0
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';
import { createServer } from 'vite';

test('the daemon answer to a switch edit is read exactly, and anything else is a failure', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { decodeSettingsResponse, editLine, saveSetting } = await server.ssrLoadModule('/src/renderer/settings/edit.ts');
    const saved = decodeSettingsResponse(200, JSON.stringify({ state: 'saved', wrote: true, applied: ['floor.answer'], restart: [] }));
    assert.deepEqual(saved, { kind: 'saved', applied: ['floor.answer'], restart: [] });
    assert.equal(editLine(saved), 'Saved. The daemon runs it now.');
    const restart = decodeSettingsResponse(200, JSON.stringify({ state: 'saved', wrote: true, applied: [], restart: ['floor.terminal'] }));
    assert.match(editLine(restart), /floor\.terminal takes a restart/);
    const refused = decodeSettingsResponse(409, JSON.stringify({ state: 'refused', reason: 'start_rule', detail: 'floor.goal on needs floor.control on' }));
    assert.match(editLine(refused), /floor\.goal on needs floor\.control on/);
    const notLive = decodeSettingsResponse(200, JSON.stringify({ state: 'saved_not_live', reason: 'enrollment_drift', wrote: true, applied: [], restart: [] }));
    assert.deepEqual(notLive, { kind: 'not_live', reason: 'enrollment_drift' });
    assert.match(editLine(notLive), /not running yet[^]*next restart/);
    assert.doesNotMatch(editLine(notLive), /runs it now/);
    assert.match(editLine(decodeSettingsResponse(409, JSON.stringify({ state: 'refused', reason: 'zeroes_reserve', detail: null }))), /per-turn reserve to zero/);
    assert.deepEqual(decodeSettingsResponse(200, 'not json'), { kind: 'failed' });
    assert.deepEqual(decodeSettingsResponse(500, '{}'), { kind: 'failed' });
    assert.deepEqual(decodeSettingsResponse(200, JSON.stringify({ state: 'saved', applied: 'x', restart: [] })), { kind: 'failed' });

    const sent = [];
    const fetch = (url, init) => { sent.push([url, init]); return Promise.resolve({ status: 409, text: () => Promise.resolve(JSON.stringify({ state: 'refused', reason: 'key_not_editable', detail: null })) }); };
    const result = await saveSetting('providers.pi.command', 'on', fetch, 'csrf-value');
    assert.deepEqual(result, { kind: 'refused', reason: 'key_not_editable', detail: null });
    assert.equal(sent[0][0], '/floor/settings');
    assert.equal(sent[0][1].headers['X-Dudes-CSRF'], 'csrf-value');
    assert.equal(sent[0][1].body, '{"key":"providers.pi.command","value":"on"}');
    assert.deepEqual(await saveSetting('floor.answer', 'on', fetch, null), { kind: 'failed' });
  } finally {
    await server.close();
  }
});
