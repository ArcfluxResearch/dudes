// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Mutation controls, each named by the test it turns red:
//   - the Hire switch ignored ................... "Hire says it is off…"
//   - the office pin not read ................... "PIN_UNSHOWN mutant…"
import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { applyServerFrame, beginFloorEpoch, getFloorState, noteTransport, resetFloorStore } from '../src/renderer/floor/store.ts';

const REPORT = {
  folder_picker: false, switches: { answer: false, control: false, recovery: false, goal: false, retire: false, terminal: false, hire: false, git: false, renew: false, board: false, diff: false, project: false },
  budget: { floor_usd_per_day_micros: '0', per_turn_usd_micros: '1234', per_turn_tokens: '9007199254740993' },
  providers: ['claude', 'codex'],
  workspaces: [{ workspace_id: 'repo_1', label: '<Service>', agent_usd_per_day_micros: '10000000',
    agent_tokens_per_day: null, goal_usd_micros: '5000000', goal_tokens: '456' }],
  about: { daemon_version: '0.12.3', browser_wire_version: 6 },
};

function floor(report = REPORT) {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  noteTransport('open');
  applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [] }, settings: report }), epoch);
  return getFloorState();
}

test('Settings shows read-only running values, local commands, Setup and About', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { SettingsView, UnavailableControls } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx');
    const html = renderToStaticMarkup(createElement(SettingsView, { floor: floor(), onSetup() {} }));
    for (const expected of ['Floor switches', '$0', '$0.001234', '9007199254740993',
      'default to $0.25 and 20000 tokens', '&lt;Service&gt;', '$10', '$5', '456',
      'Setup', 'About', 'Daemon', '0.12.3', 'Floor build', 'Browser wire', 'browser-wire-v6', 'restart']) {
      assert.ok(html.includes(expected), `missing ${expected}: ${html}`);
    }
    assert.doesNotMatch(html, /dudes config|dudes project (?:add|list|remove)|--completion-check-label/);
    const collapsed = html.replace(/<details[^>]*>[\s\S]*?<\/details>/g, '');
    assert.doesNotMatch(collapsed, /floor\.[a-z]+|browser-wire|dudes (?:project|quickstart|upgrade)/);
    assert.deepEqual([...html.matchAll(/<h3[^>]*>([^<]+)<\/h3>/g)].map(match => match[1]), [
      'Engines', 'Projects', 'Spending limits', 'Desktop notifications', 'Signed-in browsers', 'Floor switches', 'Office layout', 'About',
    ]);
    assert.match(html, /<details><summary>Configuration details<\/summary><dl>[\s\S]*<code>floor\.git<\/code>/);

    const anchors = [...html.matchAll(/<a ([^>]*)>/g)].map((match) => match[1]).filter((attrs) => attrs.includes('github.com'));
    const link = (href) => `href="${href}" target="_blank" rel="noopener noreferrer"`;
    assert.deepEqual(anchors, [
      link('https://github.com/ArcfluxResearch/dudes/releases/tag/v0.12.3'),
      link('https://github.com/ArcfluxResearch/dudes'),
      link('https://github.com/ArcfluxResearch/dudes/issues'),
    ]);
    assert.match(html, /<details><summary>Update and restart details<\/summary><p>To check for a newer version, run <code>dudes upgrade --check<\/code>/);
    assert.match(html, />Release notes for 0\.12\.3<\/a>/);
    for (const version of ['0.89.0-dev', '0.89.0-rc.1', '0.89.0+build.5']) {
      const dev = renderToStaticMarkup(createElement(SettingsView,
        { floor: floor({ ...REPORT, about: { ...REPORT.about, daemon_version: version } }), onSetup() {} }));
      assert.ok(dev.includes(`<a href="https://github.com/ArcfluxResearch/dudes/releases" target="_blank" rel="noopener noreferrer">Release notes</a>`), version);
      assert.ok(!dev.includes('/releases/tag/'), version);
    }
    assert.match(html, /Chat messages<\/dt><dd>Always on, with no setting<\/dd>/);
    assert.doesNotMatch(html, /floor\.chat/);
    const gitRow = (git) => renderToStaticMarkup(createElement(SettingsView,
      { floor: floor({ ...REPORT, switches: { ...REPORT.switches, git } }), onSetup() {} }));
    assert.match(gitRow(false), /<dt>Show changed files<\/dt><dd><span class="settings-says">[^<]*<\/span> Off <button type="button">Turn on<\/button><\/dd>/);
    assert.match(gitRow(true), /<dt>Show changed files<\/dt><dd><span class="settings-says">[^<]*<\/span> On <button type="button">Turn off<\/button><\/dd>/);
    assert.match(html, /<dt>Start new goals<\/dt><dd><span class="settings-says">[^<]*<\/span> Off It needs Change goals and Hire agents\. <button type="button">Turn on<\/button><\/dd>/);
    const renewOn = renderToStaticMarkup(createElement(SettingsView,
      { floor: floor({ ...REPORT, switches: { ...REPORT.switches, renew: true } }), onSetup() {} }));
    assert.match(renewOn, /<dt>Start new goals<\/dt><dd><span class="settings-says">[^<]*<\/span> On <button type="button">Turn off<\/button><\/dd>/);
    const hint = (control) => renderToStaticMarkup(createElement(UnavailableControls, { floor: floor(), control }));
    assert.match(hint('retire'), /Turn on Pause, resume and rename and Retire agents under\s+Floor switches in Settings/);
    assert.match(hint('terminal'), /turn on Attach terminals under Floor switches in Settings, then restart Dudes/);

    assert.match(html, /Manage engines, projects and spending/);
    assert.match(html, /Check which engines the daemon can find\. Sign in when you hire an agent\./);
    assert.doesNotMatch(html, /Check whether each engine is installed and signed in/);
    const diffRow = (git) => renderToStaticMarkup(createElement(SettingsView,
      { floor: floor({ ...REPORT, switches: { ...REPORT.switches, diff: true, git } }), onSetup() {} }));
    assert.match(diffRow(false), /<dt>Read ticket changes<\/dt><dd><span class="settings-says">[^<]*<\/span> On, but it needs Show changed files /);
    assert.match(diffRow(true), /<dt>Read ticket changes<\/dt><dd><span class="settings-says">[^<]*<\/span> On <button/);
    noteTransport('close');
    const closed = renderToStaticMarkup(createElement(SettingsView, { floor: getFloorState(), onSetup() {} }));
    assert.match(closed, /Settings is unavailable while the stream is disconnected/);
    assert.match(closed, /<button[^>]*>Retry<\/button>/);
    assert.doesNotMatch(closed, /Waiting for a snapshot/);
    assert.doesNotMatch(closed, /0.12.3/);
  } finally { await server.close(); }
});

test('Hire says it is off when its switch is off, and names every other reason when it is on', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { UnavailableControls } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx');
    const hint = (hire) => renderToStaticMarkup(createElement(UnavailableControls,
      { floor: floor({ ...REPORT, switches: { ...REPORT.switches, hire } }), control: 'hire' }));
    assert.equal(hint(false), '<p class="settings-guidance">Hire is off. Turn on Hire agents under Floor '
      + 'switches in Settings.</p>');
    assert.equal(hint(true), '<p class="settings-guidance">Hire is unavailable. Check Engines and Projects in Settings.</p>');
  } finally { await server.close(); }
});

test('Settings offers a Remove control for every project, whatever its id, and no terminal command to remove one', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { SettingsView } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx');
    for (const id of ['-h', '--help', '--json', '--', '-ordinary']) {
      for (const project of [false, true]) {
        const report = { ...REPORT, switches: { ...REPORT.switches, project },
          budget: { ...REPORT.budget, floor_usd_per_day_micros: null },
          workspaces: [{ ...REPORT.workspaces[0], workspace_id: id }] };
        const html = renderToStaticMarkup(createElement(SettingsView, { floor: floor(report), onSetup() {} }));
        assert.equal(html.includes('>Remove project</button>'), project, `${id} with the switch ${project}`);
        assert.doesNotMatch(html, /dudes project remove/);
        assert.doesNotMatch(html, /Before enabling Control, Goal or Retire/);
        assert.match(html, /No ceiling/);
      }
    }
  } finally { await server.close(); }
});

test('PIN_UNSHOWN mutant: Settings offers Automatic and each office, and presses the one this browser pins', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { SettingsView } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx');
    const { setOfficePin } = await server.ssrLoadModule('/src/renderer/view.ts');
    const pressed = () => [...renderToStaticMarkup(createElement(SettingsView, { floor: floor(), onSetup() {} }))
      .matchAll(/<button type="button" aria-pressed="(true|false)">([^<]+)<\/button>/g)].map(([, on, label]) => `${label}: ${on}`);
    assert.deepEqual(pressed(), ['Automatic: true', 'Studio, 8 desks: false', 'Workshop, 12 desks: false', 'HQ, 16 desks: false']);
    setOfficePin('workshop');
    assert.deepEqual(pressed(), ['Automatic: false', 'Studio, 8 desks: false', 'Workshop, 12 desks: true', 'HQ, 16 desks: false']);
    setOfficePin(null);
    assert.deepEqual(pressed(), ['Automatic: true', 'Studio, 8 desks: false', 'Workshop, 12 desks: false', 'HQ, 16 desks: false']);
  } finally { await server.close(); }
});

test('Settings asks on the page before the terminal goes on or retire or control goes off', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { SettingsView } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx');
    const edit = await server.ssrLoadModule('/src/renderer/settings/edit.ts');
    const on = { ...REPORT.switches, control: true, retire: true };
    const view = () => renderToStaticMarkup(createElement(SettingsView, { floor: floor({ ...REPORT, switches: on }), onSetup() {} }));
    assert.doesNotMatch(view(), /aria-label="Turn (on|off) floor\./);

    await edit.changeSetting('floor.terminal', 'on');
    assert.equal(edit.getSettingsEditState().asking?.key, 'floor.terminal');
    assert.equal(edit.getSettingsEditState().sending, null, 'nothing was sent yet');
    assert.match(view(), /aria-label="Turn on Attach terminals"> [^<]*takes effect after a restart of the daemon\. <button type="button">Turn on<\/button> <button type="button">Cancel<\/button>/);
    edit.cancelSetting();
    assert.equal(edit.getSettingsEditState().asking, null);
    assert.doesNotMatch(view(), /aria-label="Turn (on|off) floor\./);

    for (const key of ['floor.retire', 'floor.control']) {
      await edit.changeSetting(key, 'off');
      assert.equal(edit.getSettingsEditState().asking?.key, key);
      assert.match(view(), new RegExp(`aria-label="Turn off ${key === 'floor.retire' ? 'Retire agents' : 'Pause, resume and rename'}"`));
      edit.cancelSetting();
    }

    await edit.changeSetting('floor.answer', 'on');
    assert.equal(edit.getSettingsEditState().asking, null, 'a plain switch is written without a question');
    edit.resetSettingsEdit();
  } finally {
    await server.close();
  }
});

test('Settings has a button for each engine and a box for each spending limit, and no command for either', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { SettingsView } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx');
    const render = (providers) => renderToStaticMarkup(createElement(SettingsView,
      { floor: floor({ ...REPORT, providers }), onSetup() {} }));
    const off = render(['claude']);
    assert.match(off, /Codex is off\.<\/span><button type="button">Turn on<\/button>/);
    assert.match(off, /Pi is off\.<\/span><button type="button">Turn on<\/button>/);
    const on = render(['claude', 'codex', 'pi']);
    assert.match(on, /Codex is on\.<\/span><button type="button">Turn off<\/button>/);
    assert.equal(on.match(/<form class="settings-limit">/g).length, 3);
    assert.doesNotMatch(on, /cannot remove a limit once it is set/);
    assert.equal(on.match(/>Clear limit<\/button>/g)?.length, 1);
    assert.doesNotMatch(on, /dudes config|Register optional providers/);
    const edit = await server.ssrLoadModule('/src/renderer/settings/edit.ts');
    const change = edit.limitChange('usd', '7.5', '5000000');
    await edit.changeSetting('budget.floor_usd_per_day', change.value, change.question, 'Raise limit');
    assert.match(render(['claude']), /aria-label="Change Floor daily dollars"> This raises the limit from \$5 to \$7\.5\. <button type="button">Raise limit<\/button> <button type="button">Cancel<\/button>/);
    assert.equal(edit.getSettingsEditState().sending, null, 'nothing is written until the person confirms');
    edit.resetSettingsEdit();
  } finally { await server.close(); }
});

test('a saved daily ceiling clear stays beside the running value until replaced or confirmed', async () => {
  const root = resolve(import.meta.dirname, '..');
  const server = await createServer({ root, server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  try {
    const { SettingsView } = await server.ssrLoadModule('/src/renderer/settings/SettingsView.tsx');
    const edit = await server.ssrLoadModule('/src/renderer/settings/edit.ts');
    const store = await server.ssrLoadModule('/src/renderer/floor/store.ts');
    edit.resetSettingsEdit();
    const report = { ...REPORT, budget: { ...REPORT.budget, floor_usd_per_day_micros: '5000000' } };
    const render = (current = report) => renderToStaticMarkup(createElement(SettingsView, { floor: floor(current), onSetup() {} }));
    const response = (state = 'saved', status = 200) => async () => ({ status,
      text: async () => JSON.stringify({ state, applied: [], restart: ['budget'], reason: 'bad_value' }) });
    const save = (key, value, fetch = response()) => edit.saveSetting(key, value, fetch, () => true);
    await save('budget.floor_usd_per_day', 'unset');
    assert.match(render(), /<dt>Floor daily dollars<\/dt><dd>\$5/);
    assert.match(render(), /Saved from this page\. No daily dollar ceiling after restart/);
    await save('floor.answer', 'on');
    assert.equal(edit.getSettingsEditState().pendingDailyCeilingClear, true, 'another key does not replace the clear');
    await save('budget.floor_usd_per_day', '8', response('refused', 409));
    assert.equal(edit.getSettingsEditState().pendingDailyCeilingClear, true, 'a refusal does not replace the saved clear');
    store.beginFloorEpoch();
    store.noteTransport('open');
    store.noteTransport('close');
    store.noteTransport('open');
    assert.equal(edit.getSettingsEditState().pendingDailyCeilingClear, true, 'reconnection does not prove a new running ceiling');
    await save('budget.floor_usd_per_day', '8');
    assert.doesNotMatch(render(), /No daily dollar ceiling after restart/);
    await save('budget.floor_usd_per_day', 'unset', response('saved_not_live'));
    assert.match(render(), /No daily dollar ceiling after restart/);
    edit.resetSettingsEdit();
    assert.doesNotMatch(render(), /No daily dollar ceiling after restart/, 'reload forgets unverifiable saved intent');
    const session = await server.ssrLoadModule('/src/session.ts');
    const signing = await server.ssrLoadModule('/src/signer.ts');
    const signer = { device: async () => 'settings-test-device', sign: async () =>
      ({ origin: 'http://127.0.0.1:7717', time: '1', request: 'test', signature: 'test' }) };
    signing.installSigner(signer);
    await session.adoptStoredFloorSession(signer);
    const savedWindow = globalThis.window;
    globalThis.window = { fetch: response() };
    try {
      await edit.changeSetting('budget.floor_usd_per_day', 'unset', 'Clear the daily ceiling?', 'Clear limit');
      await edit.confirmSetting();
    } finally { globalThis.window = savedWindow; signing.installSigner(null); session.resetFloorSession(); }
    assert.equal(edit.getSettingsEditState().last?.result.kind, 'saved');
    const epoch = store.beginFloorEpoch();
    store.noteTransport('open');
    const unlimited = { ...report, budget: { ...report.budget, floor_usd_per_day_micros: null } };
    store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 1, agents: [] }, settings: unlimited }), epoch);
    assert.equal(edit.getSettingsEditState().pendingDailyCeilingClear, false, 'a live no-ceiling snapshot confirms the clear');
    assert.equal(edit.getSettingsEditState().last, null, 'the confirmed clear no longer asks for another restart');
    assert.doesNotMatch(render(unlimited), />Clear limit<\/button>/);
    assert.match(render(unlimited), /<dt>Floor daily dollars<\/dt><dd>No ceiling/);
    store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 2, agents: [] }, settings: report }), epoch);
    await save('budget.floor_usd_per_day', 'unset');
    signing.installSigner(signer);
    await session.adoptStoredFloorSession(signer);
    globalThis.window = { fetch: response('refused', 409) };
    try { await edit.changeSetting('budget.floor_usd_per_day', '8', null); }
    finally { globalThis.window = savedWindow; signing.installSigner(null); session.resetFloorSession(); }
    const refusal = edit.getSettingsEditState().last;
    assert.equal(refusal?.result.kind, 'refused');
    assert.equal(edit.getSettingsEditState().pendingDailyCeilingClear, true);
    store.applyServerFrame(JSON.stringify({ type: 'floor.snapshot', projection: { lsn: 3, agents: [] }, settings: unlimited }), epoch);
    assert.equal(edit.getSettingsEditState().pendingDailyCeilingClear, false);
    assert.equal(edit.getSettingsEditState().last, refusal, 'confirming the earlier clear keeps a later refusal visible');
    edit.resetSettingsEdit();
  } finally { await server.close(); }
});
