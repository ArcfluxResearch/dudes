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
  switches: { answer: false, control: false, recovery: false, goal: false, retire: false, terminal: false, hire: false, git: false, renew: false, board: false, diff: false, project: false },
  budget: { floor_usd_per_day_micros: '0', per_turn_usd_micros: '1234', per_turn_tokens: '9007199254740993' },
  providers: ['claude', 'codex'],
  workspaces: [{ workspace_id: 'repo_1', label: '<Service>', agent_usd_per_day_micros: '10000000',
    agent_tokens_per_day: null, goal_usd_micros: '5000000', goal_tokens: '456' }],
  about: { daemon_version: '0.12.3', browser_wire_version: 3 },
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
      'Setup', 'About', 'Daemon', '0.12.3', 'Floor build', 'Browser wire', 'browser-wire-v3', 'restart']) {
      assert.ok(html.includes(expected), `missing ${expected}: ${html}`);
    }
    assert.doesNotMatch(html, /dudes config|dudes project (?:add|list|remove)|--completion-check-label/);
    const anchors = [...html.matchAll(/<a ([^>]*)>/g)].map((match) => match[1]).filter((attrs) => attrs.includes('github.com'));
    const link = (href) => `href="${href}" target="_blank" rel="noopener noreferrer"`;
    assert.deepEqual(anchors, [
      link('https://github.com/ArcfluxResearch/dudes/releases/tag/v0.12.3'),
      link('https://github.com/ArcfluxResearch/dudes'),
      link('https://github.com/ArcfluxResearch/dudes/issues'),
    ]);
    assert.match(html, /never checks for updates[^<]*<code>dudes upgrade --check<\/code>/);
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
    assert.match(gitRow(false), /<dt>Read checkouts in the Git tab <code>floor\.git<\/code><\/dt><dd>Off<button type="button">Turn on<\/button><\/dd>/);
    assert.match(gitRow(true), /<dt>Read checkouts in the Git tab <code>floor\.git<\/code><\/dt><dd>On<button type="button">Turn off<\/button><\/dd>/);
    assert.match(html, /<dt>Start new goals <code>floor\.renew<\/code><\/dt><dd>Off It needs goal change and Hire\.<button type="button">Turn on<\/button><\/dd>/);
    const renewOn = renderToStaticMarkup(createElement(SettingsView,
      { floor: floor({ ...REPORT, switches: { ...REPORT.switches, renew: true } }), onSetup() {} }));
    assert.match(renewOn, /<dt>Start new goals <code>floor\.renew<\/code><\/dt><dd>On<button type="button">Turn off<\/button><\/dd>/);
    const hint = (control) => renderToStaticMarkup(createElement(UnavailableControls, { floor: floor(), control }));
    assert.match(hint('retire'), /Turn on Pause, resume and rename and Retire agents under\s+Floor switches in Settings/);
    assert.match(hint('terminal'), /Turn on Attach terminals under Floor switches in Settings/);

    assert.match(html, /Turn a switch or an engine on or off here, and set the spending limits/);
    const diffRow = (git) => renderToStaticMarkup(createElement(SettingsView,
      { floor: floor({ ...REPORT, switches: { ...REPORT.switches, diff: true, git } }), onSetup() {} }));
    assert.match(diffRow(false), /floor\.diff<\/code><\/dt><dd>On, but needs Git</);
    assert.match(diffRow(true), /floor\.diff<\/code><\/dt><dd>On<button/);
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
    assert.equal(hint(true), '<p class="settings-guidance">Hire needs a project (add its folder in Settings), at '
      + 'least one engine offered for it, and a free Hire slot in this session. An engine is withdrawn when the '
      + 'daemon&#x27;s configuration turns it off, or when the daemon holds a billing key for it and the project has '
      + 'no ceiling that key requires.</p>');
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
    assert.match(view(), /aria-label="Turn on floor\.terminal"> [^<]*takes effect after a restart of the daemon\. <button type="button">Turn on<\/button> <button type="button">Cancel<\/button>/);
    edit.cancelSetting();
    assert.equal(edit.getSettingsEditState().asking, null);
    assert.doesNotMatch(view(), /aria-label="Turn (on|off) floor\./);

    for (const key of ['floor.retire', 'floor.control']) {
      await edit.changeSetting(key, 'off');
      assert.equal(edit.getSettingsEditState().asking?.key, key);
      assert.match(view(), new RegExp(`aria-label="Turn off ${key.replace('.', '\\.')}"`));
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
    assert.match(on, /cannot remove a limit once it is set/);
    assert.doesNotMatch(on, /dudes config|Register optional providers/);
    const edit = await server.ssrLoadModule('/src/renderer/settings/edit.ts');
    const change = edit.limitChange('usd', '7.5', '5000000');
    await edit.changeSetting('budget.floor_usd_per_day', change.value, change.question, 'Raise limit');
    assert.match(render(['claude']), /aria-label="Change Floor daily dollars"> This raises the limit from \$5 to \$7\.5\. <button type="button">Raise limit<\/button> <button type="button">Cancel<\/button>/);
    assert.equal(edit.getSettingsEditState().sending, null, 'nothing is written until the person confirms');
    edit.resetSettingsEdit();
  } finally { await server.close(); }
});
