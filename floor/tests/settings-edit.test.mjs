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
    const zeroed = editLine(decodeSettingsResponse(409, JSON.stringify({ state: 'refused', reason: 'zeroes_reserve', detail: null })));
    assert.match(zeroed, /per-turn reserve of zero[^]*has to stay above zero/);
    assert.doesNotMatch(zeroed, /dudes config/);
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

test('a limit asks before it goes up or is set where there was none, and never when it goes down', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { limitChange } = await server.ssrLoadModule('/src/renderer/settings/edit.ts');
    assert.deepEqual(limitChange('usd', '7.50', '5000000'),
      { kind: 'change', value: '7.5', question: 'This raises the limit from $5 to $7.5.' });
    assert.equal(limitChange('usd', '$2', '5000000').question, null);
    assert.deepEqual(limitChange('usd', '5', '5000000'), { kind: 'same' });
    assert.equal(limitChange('usd', '5', null).question, 'This sets a limit of $5 where there was none.');
    assert.equal(limitChange('tokens', '30000', '20000').question, 'This raises the limit from 20000 tokens to 30000 tokens.');
    assert.equal(limitChange('tokens', '100', '20000').question, null);
    assert.equal(limitChange('tokens', '0', '20000').value, '0');
    for (const bad of ['', 'abc', '-1', '1.2345678', '1e3', '2.5']) {
      assert.deepEqual(limitChange(bad === '2.5' ? 'tokens' : 'usd', bad, '1'), { kind: 'invalid' }, bad);
    }
  } finally { await server.close(); }
});
