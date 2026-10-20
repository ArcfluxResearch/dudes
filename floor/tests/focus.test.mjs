// SPDX-License-Identifier: MIT OR Apache-2.0

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';
import test from 'node:test';

import ts from 'typescript';

import { handFocusOnLeave } from '../src/renderer/focus.ts';

function jsxElementsIn(text, file) {
  const syntax = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  assert.deepEqual(syntax.parseDiagnostics, [], `${file} no longer parses`);
  const elements = [];
  (function visit(node) {
    if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
      const props = {};
      for (const entry of node.attributes.properties.filter(ts.isJsxAttribute)) {
        const value = entry.initializer;
        props[entry.name.getText(syntax)] = value === undefined ? ''
          : ts.isStringLiteral(value) ? value.text : value.expression?.getText(syntax);
      }
      elements.push({ tag: node.tagName.getText(syntax), props });
    }
    ts.forEachChild(node, visit);
  })(syntax);
  return elements;
}

function pageWithHeadings(...ids) {
  const document = { activeElement: null, getElementById: (id) => headings.get(id) ?? null };
  const headings = new Map(ids.map((id) => [id, { id, focus() { document.activeElement = this; } }]));
  const element = (focusOnLeave) => ({
    dataset: focusOnLeave === undefined ? {} : { focusOnLeave },
    ownerDocument: document,
    contains(other) { return other === this; },
    focus() { document.activeElement = this; },
  });
  return { document, headings, element };
}

test('a focused element that leaves the page hands the keyboard to the heading it names', () => {
  const { document, headings, element } = pageWithHeadings('control-status-title', 'agent-detail-title');
  const slot = element('agent-detail-title');
  const leave = handFocusOnLeave(slot);
  slot.focus();
  leave();
  assert.equal(document.activeElement, headings.get('agent-detail-title'));
});

test('the heading is read when the element leaves, as it last rendered, not when it arrived', () => {
  const { document, headings, element } = pageWithHeadings('control-status-title', 'agent-detail-title');
  const slot = element('agent-detail-title');
  const leave = handFocusOnLeave(slot);
  slot.dataset.focusOnLeave = 'control-status-title';
  slot.focus();
  leave();
  assert.equal(document.activeElement, headings.get('control-status-title'));
});

test('an element that does not hold the keyboard moves nothing when it leaves', () => {
  const { document, element } = pageWithHeadings('control-status-title');
  const elsewhere = element(undefined);
  elsewhere.focus();
  handFocusOnLeave(element('control-status-title'))();
  assert.equal(document.activeElement, elsewhere);
});

test('a detail remount hands focus from a descendant to the surviving view tab', () => {
  const { document, headings, element } = pageWithHeadings('overview-tab');
  const detail = element('overview-tab');
  const field = element(undefined);
  detail.contains = (other) => other === detail || other === field;
  field.focus();
  handFocusOnLeave(detail)();
  assert.equal(document.activeElement, headings.get('overview-tab'));
});

test('a focused element that names no heading, or one not on the page, moves nothing', () => {
  const { document, element } = pageWithHeadings('control-status-title');
  for (const named of [undefined, 'answer-status-title']) {
    const field = element(named);
    const leave = handFocusOnLeave(field);
    field.focus();
    leave();
    assert.equal(document.activeElement, field, `named ${named}`);
  }
});

test('each element that can leave while focused passes handFocusOnLeave itself as its ref, so the hand-off runs only when it leaves', async () => {
  const elementsOf = async (path) =>
    jsxElementsIn(await readFile(resolve(import.meta.dirname, '../src/renderer', path), 'utf8'), path);
  const answer = await elementsOf('answer/AnswerPanel.tsx');
  const control = await elementsOf('control/ControlPanel.tsx');
  const goal = await elementsOf('goal/GoalPanel.tsx');
  const notices = await elementsOf('DoneNotices.tsx');
  for (const [elements, tag, prop, value, handOff = 'handFocusOnLeave'] of [
    [answer, 'textarea', 'name', 'answer'],
    [answer, 'button', 'className', 'answer-send'],
    [control, 'button', 'key', 'pause'],
    [control, 'button', 'key', 'resume'],
    [goal, 'textarea', 'name', 'goal'],
    [goal, 'button', 'className', 'goal-send'],
    [notices, 'button', 'id', 'dismissId(notice)', 'handFocusAfterLeave'],
  ]) {
    const named = `<${tag} ${prop}="${value}">`;
    const found = elements.filter((element) => element.tag === tag && element.props[prop] === value);
    assert.equal(found.length, 1, `${named} is not in the source exactly once`);
    assert.equal(found[0].props.ref, handOff, `${named} does not pass ${handOff} itself as its ref`);
  }
});

test('focus mode hands the keyboard to Focus agent when Escape closes it from the roster or the splitter', async () => {
  const shell = jsxElementsIn(await readFile(resolve(import.meta.dirname, '../src/shell.tsx'), 'utf8'), 'shell.tsx');
  const mode = shell.filter((entry) => entry.tag === 'section' && entry.props.className === 'focus-mode');
  assert.equal(mode.length, 1, 'the focus mode section is not in the shell exactly once');
  assert.equal(mode[0].props.ref, 'handFocusOnLeave', 'focus mode drops a focused roster row to the page body when it closes');
  assert.equal(mode[0].props['data-focus-on-leave'], 'focus-toggle');
  assert.ok(shell.some((entry) => entry.tag === 'button' && entry.props.id === 'focus-toggle'), 'the hand-off target is not in the shell');
});

test('controls that leave when pressed name a surviving focus target', async () => {
  const elementsOf = async (path) =>
    jsxElementsIn(await readFile(resolve(import.meta.dirname, '../src/renderer', path), 'utf8'), path);
  for (const [path, click, target] of [
    ['LiveWork.tsx', '() => selectOutputAgent(output.targetId, true)', 'selection-clear'],
    ['LiveWork.tsx', 'onClear', 'leaveFocusTo'],
    ['onboarding/HireWizard.tsx', 'onClose', 'hire-open'],
    ['onboarding/HireWizard.tsx',
      '() => { if (blocked === null && !frozen && !hire.sending) void submitHire(floor, browserHireDeps); }',
      'hire-title'],
    ['onboarding/HireWizard.tsx',
      '() => { if (!hire.sending && !waiting) void checkHireStatus(browserHireDeps); }', 'hire-title'],
    ['onboarding/HireWizard.tsx', '() => { if (!hire.sending) releaseAttempt(); }', 'hire-title'],
    ['onboarding/HireWizard.tsx',
      '() => { if (fresh && floor.onboarding !== null) startAnotherHire(); }',
      'hire-title'],
    ['answer/AnswerPanel.tsx', 'releaseAnswer', 'leaveFocusTo'],
    ['control/PauseAllControl.tsx', 'onDismiss', 'leaveFocusTo'],
    ['answer/AnswerPanel.tsx',
      '() => { if (!answer.sending && floor.answering) void checkAnswerStatus(browserAnswerDeps); }',
      'answer-status-title'],
    ['control/ControlPanel.tsx',
      '() => { if (!control.sending && floor.control) void checkControlStatus(browserControlDeps); }',
      'control-status-title'],
    ['goal/GoalPanel.tsx',
      '() => { if (!goal.sending && floor.goalChange) void checkGoalStatus(browserGoalDeps); }',
      'goal-status-title'],
  ]) {
    const found = (await elementsOf(path)).filter((entry) => entry.tag === 'button' && entry.props.onClick === click);
    assert.equal(found.length, path === 'onboarding/HireWizard.tsx' && click === 'onClose' ? 3 : 1,
      `${path} ${click} has the wrong number of controls`);
    for (const entry of found) {
      assert.equal(entry.props.ref, 'handFocusOnLeave', `${path} ${click} loses focused button`);
      assert.equal(entry.props['data-focus-on-leave'], target, `${path} ${click} has no surviving target`);
    }
  }
  const heading = (await elementsOf('onboarding/HireWizard.tsx'))
    .find((entry) => entry.tag === 'h2' && entry.props.id === 'hire-title');
  assert.equal(heading?.props.tabIndex, '-1', 'the Hire target cannot receive focus');
});

test('buttons that disable while focused keep their native focus', async () => {
  const elementsOf = async (path) =>
    jsxElementsIn(await readFile(resolve(import.meta.dirname, '../src/renderer', path), 'utf8'), path);
  for (const [path, click] of [
    ['answer/AnswerPanel.tsx',
      '() => { if (!answer.sending && floor.answering) void checkAnswerStatus(browserAnswerDeps); }'],
    ['control/ControlPanel.tsx',
      '() => { if (!control.sending && floor.control) void checkControlStatus(browserControlDeps); }'],
    ['goal/GoalPanel.tsx',
      '() => { if (!goal.sending && floor.goalChange) void checkGoalStatus(browserGoalDeps); }'],
    ['onboarding/HireWizard.tsx', '() => { if (at !== 0 && !frozen && !stuck) moveSection(-1, floor); }'],
    ['onboarding/HireWizard.tsx',
      '() => { if (!frozen && !stuck) moveSection(1, floor); }'],
    ['onboarding/HireWizard.tsx',
      '() => { if (blocked === null && !frozen && !hire.sending) void submitHire(floor, browserHireDeps); }'],
    ['onboarding/HireWizard.tsx', '() => { if (!hire.sending && !waiting) void checkHireStatus(browserHireDeps); }'],
    ['onboarding/HireWizard.tsx', '() => { if (!hire.sending) releaseAttempt(); }'],
    ['onboarding/HireWizard.tsx',
      '() => { if (fresh && floor.onboarding !== null) startAnotherHire(); }'],
  ]) {
    const found = (await elementsOf(path)).filter((entry) => entry.tag === 'button' && entry.props.onClick === click);
    assert.equal(found.length, 1, `${path} ${click} is not unique`);
    assert.equal(found[0].props.disabled, undefined, `${path} ${click} can drop native focus`);
    assert.notEqual(found[0].props['aria-disabled'], undefined, `${path} ${click} lacks disabled state`);
  }
});

test('a held Enter on Send answer cannot repeat after a no-effect refusal', async () => {
  const answer = jsxElementsIn(await readFile(resolve(import.meta.dirname,
    '../src/renderer/answer/AnswerPanel.tsx'), 'utf8'), 'AnswerPanel.tsx');
  const send = answer.find((entry) => entry.tag === 'button' && entry.props.className === 'answer-send');
  assert.equal(send?.props.onKeyDown, '(event) => { if (event.repeat) event.preventDefault(); }');
});
