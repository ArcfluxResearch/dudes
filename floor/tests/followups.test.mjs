// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';
import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import ts from 'typescript';

const root = resolve(import.meta.dirname, '..');
const id = '01ARZ3NDEKTSV4RRFFQ69G5FAV';

test('ARCHIVE_HIRE_SWITCH mutant disables Hire again when the switch is off', async () => {
  const server = await createServer({ root, cacheDir: resolve(root, 'node_modules/.vite-followups'),
    server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    const { ArchivedAgents } = await server.ssrLoadModule('/src/renderer/ArchivedAgents.tsx');
    const { getFloorState } = await server.ssrLoadModule('/src/renderer/floor/store.ts');
    const floor = { ...getFloorState(), phase: 'live', stale: false, epoch: 1, snapshotEpoch: 1,
      onboarding: {}, retiredAgents: [{ id, name: 'Ada', role: 'engineer', provider: 'claude',
        hired_at: 1, retired_at: 2, turns: 1, spend: { usd: 0, tokens: 0, work_tokens: 0 } }] };
    function button(state) {
      const html = renderToStaticMarkup(createElement(ArchivedAgents, { floor: state, onHireAgain() {} }));
      return html.match(/<button[^>]*>Hire again<\/button>/)?.[0];
    }
    assert.match(button({ ...floor, settings: { switches: { hire: false } } }), /disabled=""/);
    assert.doesNotMatch(button({ ...floor, settings: { switches: { hire: true } } }), /disabled/);
    assert.doesNotMatch(button({ ...floor, settings: null }), /disabled/);
    assert.match(button({ ...floor, settings: null, onboarding: null }), /disabled=""/);
    assert.match(button({ ...floor, stale: true, settings: { switches: { hire: true } } }), /disabled=""/);
  } finally { await server.close(); }
});

test('BACKDROP_PRESS_RELEASE and TOUCH_BACKDROP_CAPTURE mutants dismiss only a complete primary backdrop gesture', async () => {
  const shell = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  const syntax = ts.createSourceFile('shell.tsx', shell, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let dialog;
  function visit(node) {
    if (ts.isJsxOpeningElement(node) && node.tagName.getText(syntax) === 'dialog') dialog = node;
    ts.forEachChild(node, visit);
  }
  visit(syntax);
  const props = Object.fromEntries(dialog.attributes.properties.filter(ts.isJsxAttribute)
    .map((entry) => [entry.name.getText(syntax), entry.initializer?.expression?.getText(syntax)]));
  assert.equal(props.onClick, undefined);
  const backdropPress = { current: null };
  let hit;
  const document = { elementFromPoint() { return hit; } };
  let closes = 0;
  const handlers = Object.fromEntries(['onPointerDown', 'onPointerUp', 'onPointerCancel'].map((key) => {
    assert.equal(typeof props[key], 'string', `${key} is required`);
    return [key, Function('backdropPress', 'closeHire', 'document', `return ${props[key]}`)(backdropPress, () => { closes += 1; }, document)];
  }));
  const backdrop = { ownerDocument: document };
  const content = {};
  const event = (target, extra = {}) => {
    hit = target;
    return { currentTarget: backdrop, target, pointerId: 1, clientX: 20, clientY: 30,
      button: 0, isPrimary: true, ...extra };
  };
  for (const [down, up] of [[content, backdrop], [backdrop, content], [content, content]]) {
    handlers.onPointerDown(event(down)); handlers.onPointerUp(event(up)); assert.equal(closes, 0);
  }
  handlers.onPointerDown(event(backdrop)); handlers.onPointerCancel(event(backdrop));
  handlers.onPointerUp(event(backdrop)); assert.equal(closes, 0);
  handlers.onPointerDown(event(backdrop)); handlers.onPointerUp(event(backdrop, { pointerId: 2 })); assert.equal(closes, 0);
  handlers.onPointerDown(event(backdrop, { button: 2 })); handlers.onPointerUp(event(backdrop, { button: 2 })); assert.equal(closes, 0);
  handlers.onPointerDown(event(backdrop, { isPrimary: false })); handlers.onPointerUp(event(backdrop, { isPrimary: false })); assert.equal(closes, 0);
  handlers.onPointerDown(event(backdrop)); handlers.onPointerUp(event(backdrop)); assert.equal(closes, 1);
  handlers.onPointerUp(event(backdrop)); assert.equal(closes, 1);
  handlers.onPointerDown(event(backdrop));
  const capturedRelease = event(backdrop);
  hit = content;
  handlers.onPointerUp(capturedRelease);
  assert.equal(closes, 1);
});

test('DUPLICATE_PROJECTION_PLAN and RESYNC_GAP mutants keep the route from one update, reconcile expiry and hold through a resync, '
  + 'and OFFICE_SWITCH_SKIPPED and OFFICE_SWITCH_WALKS start a new office afresh', async () => {
  const source = await readFile(resolve(root, 'src/renderer/OfficeCanvas.tsx'), 'utf8');
  const syntax = ts.createSourceFile('OfficeCanvas.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  let planner;
  function visit(node) {
    if (ts.isFunctionDeclaration(node) && node.name?.text === 'planMotion') planner = node;
    ts.forEachChild(node, visit);
  }
  visit(syntax);
  const { planCast, charactersAt } = await import('../src/renderer/scene/motion.ts');
  const { castFrom } = await import('../src/renderer/scene/cast.ts');
  const { applyTransitions, rosterTransitions } = await import('../src/renderer/scene/presence.ts');
  const { officePlan } = await import('../src/renderer/office3d/plan.ts');
  const { STUDIO } = await import('../src/renderer/office3d/studio.ts');
  let office = { plan: officePlan(STUDIO) };
  const motionsRef = { current: [] };
  const planInputsRef = { current: null };
  const expiryRef = { current: 0 };
  const now = 1_780_000_000_000;
  let count = 0;
  const code = ts.transpile(planner.getText(syntax), { target: ts.ScriptTarget.ES2022 });
  const plan = Function('choiceFor', 'built', 'motionsRef', 'planInputsRef', 'expiryRef', 'Date', 'performance',
    'planCast', 'desksOf', 'scheduleRef', 'charactersAt', 'rosterTransitions', 'applyTransitions', 'rosterRef', 'walkOut',
    'exitMotions', 'drawingRef', 'settle', 'dropExits', `${code}; return planMotion;`)(() => ({ id: 'chosen' }), () => office, motionsRef,
    planInputsRef, expiryRef, { now: () => now }, { now: () => now }, (...args) => {
      count += 1;
      return planCast(...args);
    }, (agents, chosen) => castFrom(agents, chosen.plan), { current() {} }, charactersAt, rosterTransitions, applyTransitions,
    { current: new Map() }, () => {}, () => [], { current: true }, () => {}, () => {});
  const floor = { epoch: 1, snapshotEpoch: 1, phase: 'live', stale: false,
    agents: [{ id, status: 'running' }] };
  plan(floor, 'snapshot');
  assert.equal(count, 1);
  assert.equal(motionsRef.current[0].place, 'desk');
  const changed = { ...floor, agents: [{ ...floor.agents[0],
    live_activity: { kind: 'compacting', at: now, expires_in_ms: 60_000 } }] };
  plan(changed, 'delta', true);
  const route = motionsRef.current;
  assert.equal(route[0].place, 'kitchen');
  assert.equal(route[0].travel.length, 12);
  plan(changed, 'delta');
  assert.equal(count, 2);
  assert.equal(motionsRef.current, route);
  assert.deepEqual(charactersAt(motionsRef.current, now, true)[0].foot, { x: 13.5, y: 1.5 });
  expiryRef.current = 1;
  plan(changed, 'delta');
  assert.equal(count, 3);
  plan(changed, 'snapshot', true);
  assert.equal(count, 4);
  assert.equal(motionsRef.current[0].travel.length, 1);
  plan({ ...changed, stale: true }, 'delta');
  assert.equal(count, 5);
  assert.equal(motionsRef.current[0].place, 'desk');
  const held = motionsRef.current;
  plan({ ...changed, epoch: 2, phase: 'live', agents: [] }, 'delta', true);
  assert.equal(count, 5, 'rows no snapshot confirmed, such as the empty view a resync leaves, plan nothing');
  assert.equal(motionsRef.current, held);
  plan(changed, 'delta');
  assert.equal(count, 6);
  const { WORKSHOP } = await import('../src/renderer/office3d/workshop.ts');
  office = { plan: officePlan(WORKSHOP) };
  plan(changed, 'delta');
  assert.equal(count, 7);
  assert.equal(motionsRef.current[0].travel.length, 1, 'in a new office the agent starts at its place, with no walk from the old room');
  assert.deepEqual(motionsRef.current[0].station, { x: 1.5, y: 1.5 });
});

test('BACKDROP_MODAL_LIFETIME mutant clears an incomplete gesture when opening or closing', async () => {
  const source = await readFile(resolve(root, 'src/shell.tsx'), 'utf8');
  const syntax = ts.createSourceFile('shell.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const names = new Set(['openHire', 'hireAgain', 'closeHire']);
  const functions = [];
  function visit(node) {
    if (ts.isFunctionDeclaration(node) && names.has(node.name?.text)) functions.push(node.getText(syntax));
    ts.forEachChild(node, visit);
  }
  visit(syntax);
  const backdropPress = { current: 5 };
  const code = ts.transpile(functions.join('\n'), { target: ts.ScriptTarget.ES2022 });
  const transitions = Function('backdropPress', 'hireOpener', 'hireDialog', 'document',
    'restoreFocusAfterDialog', 'setHiring', 'setArchiveHireKept', 'prepareArchiveHire', 'prepareHire', 'setGuide', 'floor',
    `${code}; return { openHire, hireAgain, closeHire };`)(backdropPress, { current: null },
    { current: { close() {} } }, { getElementById() { return null; } }, () => {}, () => {}, () => {},
    () => true, () => {}, () => {}, {});
  for (const transition of Object.values(transitions)) {
    backdropPress.current = 5;
    transition({}, {});
    assert.equal(backdropPress.current, null);
  }
});

/**
 * The canvas's own planner, lifted out of OfficeCanvas.tsx with its presence
 * helpers and its office choice: `chooseOffice` over the last crew a snapshot
 * confirmed, with this browser's pin.
 */
async function presencePlanner(drawing, pin = () => null) {
  const source = await readFile(resolve(root, 'src/renderer/OfficeCanvas.tsx'), 'utf8');
  const syntax = ts.createSourceFile('OfficeCanvas.tsx', source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const wanted = ['planMotion', 'settle', 'walkOut', 'showExits', 'exitMotions', 'drawCrew', 'desksOf', 'dropExits', 'choiceFor', 'built'];
  const bodies = [];
  (function visit(node) {
    if (ts.isFunctionDeclaration(node) && wanted.includes(node.name?.text)) bodies.push(node.getText(syntax));
    ts.forEachChild(node, visit);
  })(syntax);
  assert.equal(bodies.length, wanted.length);
  const code = ts.transpile(bodies.join('\n'), { target: ts.ScriptTarget.ES2022 });
  const motion = await import('../src/renderer/scene/motion.ts');
  const presence = await import('../src/renderer/scene/presence.ts');
  const { castFrom } = await import('../src/renderer/scene/cast.ts');
  const { officePlan } = await import('../src/renderer/office3d/plan.ts');
  const { furnish } = await import('../src/renderer/office3d/furnish.ts');
  const { chooseOffice, officeOf } = await import('../src/renderer/office3d/offices.ts');
  const now = 1_780_000_000_000;
  const refs = { motionsRef: { current: [] }, planInputsRef: { current: null }, expiryRef: { current: 0 },
    rosterRef: { current: new Map() }, exitsRef: { current: new Map() }, seatsRef: { current: [] },
    crewRef: { current: { members: [] } }, rendererRef: { current: null }, drawingRef: { current: drawing },
    scheduleRef: { current() {} }, confirmedCrewRef: { current: 0 } };
  const leavers = [];
  const scope = { ...refs, BUILT: new Map(), officeOf, officePlan, furnish, chooseOffice, getOfficePin: pin,
    Date: { now: () => now }, performance: { now: () => now },
    castFrom, ...motion, ...presence, setLeavers: (rows) => leavers.push(rows.map((row) => row.id)) };
  const names = Object.keys(scope);
  const { planMotion, choiceFor } = Function(...names, `${code}; return { planMotion, choiceFor };`)(...names.map((name) => scope[name]));
  return { planMotion, choiceFor, refs, leavers };
}

const floorOf = (agents) => ({ epoch: 1, snapshotEpoch: 1, phase: 'live', stale: false, agents });
const LIN = { id: '01ARZ3NDEKTSV4RRFFQ69G5FAW', status: 'running' };

test('SETTLE_WITHOUT_DRAWING mutant: with no office drawing, hires are present and retirees gone at once', async () => {
  const office = async (drawing) => {
    const { planMotion, refs, leavers } = await presencePlanner(drawing);
    const ada = { id, status: 'running' };
    planMotion(floorOf([ada]), 'snapshot', true);
    refs.crewRef.current = { members: [{ id, look: {}, accessory: null, badge: null }] };
    planMotion(floorOf([ada, LIN]), 'delta', true);
    const hired = refs.rosterRef.current.get(LIN.id);
    planMotion(floorOf([LIN]), 'delta', true);
    return { hired, roster: [...refs.rosterRef.current], exits: [...refs.exitsRef.current.keys()], leavers: leavers.at(-1) };
  };
  assert.deepEqual(await office(false), { hired: 'present', roster: [[LIN.id, 'present']], exits: [], leavers: [] });
  assert.deepEqual(await office(true), { hired: 'entering', roster: [[id, 'leaving'], [LIN.id, 'entering']],
    exits: [id], leavers: [id] }, 'a drawn office walks them instead');
});

test('EXITS_KEPT mutant: a new office ends a walk out of the old room, and the agent leaves at once', async () => {
  let pin = null;
  const { planMotion, refs, leavers } = await presencePlanner(true, () => pin);
  const ada = { id, status: 'running' };
  planMotion(floorOf([ada, LIN]), 'snapshot', true);
  refs.crewRef.current = { members: [ada, LIN].map((row) => ({ id: row.id, look: {}, accessory: null, badge: null })) };
  planMotion(floorOf([LIN]), 'delta', true);
  assert.deepEqual([...refs.exitsRef.current.keys()], [id], 'the retiree walks out of the Studio');
  pin = 'workshop';
  planMotion(floorOf([LIN]), 'delta', true);
  assert.equal(refs.planInputsRef.current.office.layout.label, 'Workshop');
  assert.deepEqual({ roster: [...refs.rosterRef.current], exits: [...refs.exitsRef.current.keys()], leavers: leavers.at(-1) },
    { roster: [[LIN.id, 'present']], exits: [], leavers: [] });
});

const crewOf = (count) => Array.from({ length: count }, (_, k) => ({ id: `01ARZ3NDEKTSV4RRFFQ69G5F${String(k).padStart(2, '0')}`, status: 'running' }));

test('RESYNC_SWITCHES_OFFICE mutant: the empty view a resync leaves keeps the office of the last confirmed crew', async () => {
  const { planMotion, choiceFor, refs } = await presencePlanner(true);
  planMotion(floorOf(crewOf(12)), 'snapshot', true);
  assert.equal(refs.planInputsRef.current.office.layout.label, 'Workshop');
  const resync = { epoch: 2, snapshotEpoch: 1, phase: 'resyncing', stale: false, agents: [] };
  assert.deepEqual(choiceFor(resync), { id: 'workshop', full: false }, 'a resync view is not a crew of none');
  const held = refs.motionsRef.current;
  planMotion(resync, 'snapshot', true);
  assert.equal(refs.motionsRef.current, held);
  planMotion({ ...floorOf(crewOf(3)), epoch: 2, snapshotEpoch: 2 }, 'snapshot', true);
  assert.equal(refs.planInputsRef.current.office.layout.label, 'Studio', 'the next confirmed crew chooses again');
});

test('HIRE_PLACED_ON_SWITCH and OFFICE_SWITCH_WALKS mutants: the ninth hire walks in from the Workshop door while the rest are placed there', async () => {
  const { planMotion, refs } = await presencePlanner(true);
  const eight = crewOf(8);
  planMotion(floorOf(eight), 'snapshot', true);
  assert.equal(refs.planInputsRef.current.office.layout.label, 'Studio');
  const nine = crewOf(9);
  refs.crewRef.current = { members: nine.map((row) => ({ id: row.id, look: {}, accessory: null, badge: null })) };
  planMotion(floorOf(nine), 'delta', true);
  assert.equal(refs.planInputsRef.current.office.layout.label, 'Workshop');
  const motions = refs.motionsRef.current;
  assert.deepEqual(motions.at(-1).travel[0], { x: 0.5, y: 14.5 }, 'the hire starts at the Workshop door');
  assert.ok(motions.at(-1).travel.length > 1, 'and walks to its desk');
  assert.deepEqual(motions.slice(0, 8).map((motion) => motion.travel.length), Array(8).fill(1), 'the rest are placed without a walk');
});
