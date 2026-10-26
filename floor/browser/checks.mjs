// The checks that need a real browser: keyboard behaviour, focus rings, the top layer and the page policy. The markup
// each component draws is checked without a browser in tests/ui-components.test.mjs.

import assert from 'node:assert/strict';

export const POLICY_SCRIPT = `
  window.__policy = { violations: [], styles: [], styleAttributes: [] };
  document.addEventListener('securitypolicyviolation', (event) => {
    window.__policy.violations.push(event.violatedDirective + ' ' + event.blockedURI);
  });
  new MutationObserver((records) => {
    for (const record of records) for (const node of record.addedNodes) {
      if (node.nodeName === 'STYLE') window.__policy.styles.push('a style element');
    }
  }).observe(document, { childList: true, subtree: true });
  const original = Element.prototype.setAttribute;
  Element.prototype.setAttribute = function (name, value) {
    if (String(name).toLowerCase() === 'style') window.__policy.styleAttributes.push(String(value));
    return original.call(this, name, value);
  };
`;

const THEMES = ['light', 'dark'];

export function checks({ page, cdp, log }) {
  const list = [];
  const check = (name, run) => list.push({ name, run });

  const colourOf = (variable) => page.js(`(() => { const probe = document.createElement('span');
    probe.style.color = 'var(${variable})'; document.body.append(probe);
    const value = getComputedStyle(probe).color; probe.remove(); return value; })()`);

  async function theme(name) {
    await cdp.send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-color-scheme', value: name }] });
    await page.sleep(300);
    await page.settle();
    assert.equal(await page.js(`matchMedia('(prefers-color-scheme: dark)').matches`), name === 'dark');
  }

  const isOpen = (selector) => page.js(`document.querySelector(${JSON.stringify(selector)}).matches(':popover-open')`);
  const dialogOpen = () => page.js(`document.getElementById('the-dialog').open`);

  check('the page policy blocks an inline style element (so the later zero counts mean something)', async () => {
    await page.js(`(() => { const style = document.createElement('style'); style.textContent = 'body{color:red}'; document.head.append(style); style.remove(); })()`);
    await page.waitFor(`window.__policy.violations.length > 0`, 'a policy violation event');
    assert.match(await page.js(`window.__policy.violations[0]`), /^style-src/);
    await page.sleep(200);
    await page.js(`window.__policy = { violations: [], styles: [], styleAttributes: [] }`);
    log.errors.length = 0;
  });

  check('the fonts are served as files, and the page uses them', async () => {
    await page.js(`document.fonts.ready`);
    const loaded = await page.js(`[...document.fonts].filter((face) => face.status === 'loaded').map((face) => face.family)`);
    assert.ok(loaded.some((family) => /Inter/.test(family)), `Inter did not load: ${loaded}`);
    assert.ok(loaded.some((family) => /JetBrains Mono/.test(family)), `JetBrains Mono did not load: ${loaded}`);
    assert.match(await page.js(`getComputedStyle(document.getElementById('lab')).fontFamily`), /^Inter/);
  });

  for (const name of THEMES) {
    check(`${name}: a secondary button keeps its edge, each variant looks different, and hover and press are designed`, async () => {
      await theme(name);
      await page.move(0, 0);
      await page.sleep(250);
      const look = (kind) => page.js(`(() => { const s = getComputedStyle(document.querySelector('[data-case="${kind}"]'));
        return { background: s.backgroundColor, border: s.borderTopColor, width: s.borderTopWidth, shift: s.translate, text: s.color }; })()`);
      const secondary = await look('secondary');
      assert.equal(secondary.width, '1px');
      assert.equal(secondary.border, await colourOf('--border-strong'), 'the secondary edge is the strong border colour');
      const backgrounds = new Set();
      for (const kind of ['primary', 'secondary', 'ghost', 'danger']) backgrounds.add((await look(kind)).background);
      assert.equal(backgrounds.size, 4, 'the four variants are told apart by fill');
      assert.equal((await look('primary')).background, await colourOf('--accent'));
      const { x, y } = await page.centre('[data-case="primary"]');
      await page.move(x, y);
      await page.sleep(250);
      assert.equal((await look('primary')).background, await colourOf('--accent-hover'), 'hover');
      await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 });
      await page.sleep(250);
      assert.equal((await look('primary')).shift, '0px 1px', 'pressed moves down one pixel');
      await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 });
      await page.move(0, 0);
    });
  }

  check('a loading button is busy, stays in the tab order, and a press does nothing; a disabled one is out of the order', async () => {
    const before = await page.js(`document.getElementById('lab').dataset.presses`);
    await page.click('[data-case="loading"]');
    await page.focus('[data-case="loading"]');
    await page.press('Enter');
    await page.press(' ');
    assert.equal(await page.js(`document.getElementById('lab').dataset.presses`), before);
    assert.equal(await page.js(`document.querySelector('[data-case="loading"]').getAttribute('aria-busy')`), 'true');
    assert.equal(await page.js(`document.activeElement === document.querySelector('[data-case="loading"]')`), true);
    assert.equal(await page.js(`document.querySelector('[data-case="disabled"]').disabled`), true);
    assert.equal(await page.js(`getComputedStyle(document.querySelector('[data-case="disabled"]')).opacity`), '0.5');
    await page.click('[data-case="primary"]');
    assert.equal(await page.js(`document.getElementById('lab').dataset.presses`), String(Number(before) + 1), 'a normal button counts a press');
  });

  check('a field label focuses its control, the hint and error describe it, and an invalid control draws the danger edge', async () => {
    await page.click('label[for="field-name"]');
    assert.equal(await page.active(), 'field-name');
    assert.equal(await page.js(`document.getElementById(document.getElementById('field-name').getAttribute('aria-describedby')).textContent`), 'Shown on the office floor.');
    assert.equal(await page.js(`document.getElementById('field-role').getAttribute('aria-invalid')`), 'true');
    assert.equal(await page.js(`document.getElementById(document.getElementById('field-role').getAttribute('aria-describedby')).textContent`), 'Say what this agent does.');
    assert.equal(await page.js(`getComputedStyle(document.getElementById('field-role')).borderTopColor`), await colourOf('--danger'));
  });

  for (const name of THEMES) {
    check(`${name}: the switch knob renders, sits inside its track and moves when the switch turns on`, async () => {
      await theme(name);
      const measure = () => page.js(`(() => {
        const button = document.querySelector('#switches [role="switch"]');
        const knob = button.querySelector('[data-part="knob"]');
        const t = button.getBoundingClientRect(); const k = knob.getBoundingClientRect(); const s = getComputedStyle(knob);
        return { on: button.getAttribute('aria-checked'), track: getComputedStyle(button).backgroundColor,
          knobFill: s.backgroundColor, width: k.width, height: k.height, offset: k.x - t.x, inside: k.x >= t.x && k.right <= t.right && k.y >= t.y && k.bottom <= t.bottom };
      })()`);
      await page.focus('#switches [role="switch"]');
      const off = await measure();
      assert.equal(off.on, 'false');
      assert.equal(off.width, 16);
      assert.equal(off.height, 16);
      assert.equal(off.inside, true);
      assert.notEqual(off.knobFill, 'rgba(0, 0, 0, 0)', 'the knob has a fill');
      assert.equal(off.knobFill, await colourOf('--ui-surface'));
      await page.press(' ');
      await page.sleep(250);
      const on = await measure();
      assert.equal(on.on, 'true');
      assert.equal(on.inside, true);
      assert.equal(Math.round(on.offset - off.offset), 16, 'the knob travels its track');
      assert.notEqual(on.track, off.track, 'the track changes colour');
      assert.equal(on.track, await colourOf('--accent'));
      await page.press(' ');
      await page.sleep(250);
      assert.equal((await measure()).on, 'false');
    });
  }

  check('a busy switch shows the held value and ignores a press', async () => {
    const busy = '#switches [role="switch"][aria-busy="true"]';
    await page.click(busy);
    assert.equal(await page.js(`document.querySelector('${busy}').getAttribute('aria-checked')`), 'false');
  });

  check('tabs: the arrows, Home and End move the selection and the focus together, and only the selected tab is in the tab order', async () => {
    await page.focus('#tab-Overview');
    assert.equal(await page.js(`[...document.querySelectorAll('[role="tab"]')].filter((tab) => tab.tabIndex === 0).length`), 1);
    await page.press('ArrowRight');
    assert.equal(await page.js(`document.getElementById('tab-now').textContent`), 'Memory');
    assert.equal(await page.active(), 'tab-Memory');
    await page.press('End');
    assert.equal(await page.js(`document.getElementById('tab-now').textContent`), 'Git');
    await page.press('ArrowRight');
    assert.equal(await page.js(`document.getElementById('tab-now').textContent`), 'Overview', 'wraps');
    await page.press('ArrowLeft');
    assert.equal(await page.js(`document.getElementById('tab-now').textContent`), 'Git');
    await page.press('Home');
    assert.equal(await page.js(`document.getElementById('tab-now').textContent`), 'Overview');
    assert.equal(await page.js(`document.getElementById('tab-Overview').getAttribute('aria-selected')`), 'true');
  });

  check('a disclosure opens with Enter and its chevron turns', async () => {
    const summary = '#disclosure-one summary';
    const turn = () => page.js(`getComputedStyle(document.querySelector('${summary} svg')).rotate`);
    assert.equal(await page.js(`document.getElementById('disclosure-one').open`), false);
    assert.equal(await turn(), 'none');
    await page.focus(summary);
    await page.press('Enter');
    await page.sleep(250);
    assert.equal(await page.js(`document.getElementById('disclosure-one').open`), true);
    assert.equal(await turn(), '90deg');
    await page.press('Enter');
    assert.equal(await page.js(`document.getElementById('disclosure-one').open`), false);
  });

  check('reduced motion removes transitions and animations', async () => {
    const duration = () => page.js(`getComputedStyle(document.querySelector('[data-case="primary"]')).transitionDuration`);
    assert.equal(await duration(), '0.12s');
    await cdp.send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-reduced-motion', value: 'reduce' }] });
    await page.settle();
    assert.equal(await duration(), '0s');
    await cdp.send('Emulation.setEmulatedMedia', { features: [{ name: 'prefers-reduced-motion', value: 'no-preference' }] });
  });

  async function menuSuite(scope, insideDialog) {
    const trigger = `${scope} [data-ui="menu-button"]`;
    const list = `${scope} [data-ui="menu"]`;
    const item = () => page.js(`document.activeElement.textContent.trim()`);
    const closedWithFocusBack = async (why) => {
      assert.equal(await isOpen(list), false, `${why}: the list is closed`);
      assert.equal(await page.js(`document.activeElement === document.querySelector('${trigger}')`), true, `${why}: focus is back on the button`);
      if (insideDialog) assert.equal(await dialogOpen(), true, `${why}: the dialog stays open`);
    };
    await page.focus(trigger);
    await page.press('Enter');
    await page.waitFor(`document.querySelector('${list}').matches(':popover-open')`, 'the menu to open');
    await page.waitFor(`document.activeElement.getAttribute('role') === 'menuitem'`, 'focus to enter the list');
    assert.equal(await item(), 'Change model', 'Enter opens and focuses the first item');
    assert.equal(await page.js(`document.querySelector('${trigger}').getAttribute('aria-expanded')`), 'true');
    const top = await page.js(`(() => { const r = document.querySelector('${list}').getBoundingClientRect();
      const hit = document.elementFromPoint(r.x + r.width / 2, r.y + 8);
      return { onTop: hit !== null && hit.closest('[data-ui="menu"]') !== null, inView: r.x >= 0 && r.y >= 0 && r.right <= innerWidth && r.bottom <= innerHeight }; })()`);
    assert.deepEqual(top, { onTop: true, inView: true }, 'the list is drawn above everything, inside the viewport');
    await page.press('ArrowDown');
    assert.equal(await item(), 'Pause');
    await page.press('ArrowDown');
    assert.equal(await item(), 'Retire', 'the disabled item is skipped');
    await page.press('ArrowDown');
    assert.equal(await item(), 'Change model', 'wraps');
    await page.press('ArrowUp');
    assert.equal(await item(), 'Retire', 'wraps backwards');
    await page.press('Home');
    assert.equal(await item(), 'Change model');
    await page.press('End');
    assert.equal(await item(), 'Retire');
    await page.press('p');
    assert.equal(await item(), 'Pause', 'type-ahead');
    const ring = await page.js(`(() => { const s = getComputedStyle(document.activeElement); return { style: s.outlineStyle, width: s.outlineWidth, colour: s.outlineColor }; })()`);
    assert.deepEqual(ring, { style: 'solid', width: '2px', colour: await colourOf('--focus') }, 'the focused item draws the focus ring');
    await page.press('Escape');
    await closedWithFocusBack('Escape');
    await page.press('ArrowUp');
    await page.waitFor(`document.activeElement.getAttribute('role') === 'menuitem'`, 'focus to enter the list');
    assert.equal(await item(), 'Retire', 'ArrowUp on the button opens at the last item');
    await page.press('Home');
    await page.press('Enter');
    await closedWithFocusBack('choosing');
    assert.equal(await page.js(`document.getElementById('lab').dataset.last`), `${scope === '#the-dialog' ? 'dialog' : 'page'}:change-model`, 'the choice ran after the menu closed');
    await page.press('Enter');
    await page.waitFor(`document.querySelector('${list}').matches(':popover-open')`, 'the menu to open');
    const { x, y } = await page.centre('#end');
    await page.move(x, y);
    await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 });
    await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 });
    await page.settle();
    assert.equal(await isOpen(list), false, 'a press outside closes it');
  }

  check('menu, on the page: open, arrows, Home, End, type-ahead, Escape, choose, focus return', async () => {
    await theme('light');
    await menuSuite('#menus', false);
  });

  async function selectSuite(scope, insideDialog) {
    const trigger = `${scope} [data-ui="select"]`;
    const list = `${scope} [data-ui="select-list"]`;
    const activeOption = () => page.js(`(() => { const id = document.querySelector('${trigger}').getAttribute('aria-activedescendant');
      return id ? document.getElementById(id).textContent.trim() : null; })()`);
    const shown = () => page.js(`document.querySelector('${trigger}').textContent.trim()`);
    await page.focus(trigger);
    await page.press('ArrowDown');
    await page.waitFor(`document.querySelector('${list}').matches(':popover-open')`, 'the list to open');
    assert.equal(await page.js(`document.querySelector('${trigger}').getAttribute('aria-expanded')`), 'true');
    assert.equal(await page.js(`document.activeElement === document.querySelector('${trigger}')`), true, 'focus stays on the button');
    const top = await page.js(`(() => { const r = document.querySelector('${list}').getBoundingClientRect();
      const hit = document.elementFromPoint(r.x + r.width / 2, r.y + 8);
      return { onTop: hit !== null && hit.closest('[data-ui="select-list"]') !== null, inView: r.x >= 0 && r.y >= 0 && r.right <= innerWidth && r.bottom <= innerHeight }; })()`);
    assert.deepEqual(top, { onTop: true, inView: true }, 'the list is drawn above everything, inside the viewport');
    await page.press('Home');
    assert.equal(await activeOption(), 'Automatic');
    await page.press('ArrowDown');
    assert.equal(await activeOption(), 'Character C');
    await page.press('ArrowDown');
    assert.equal(await activeOption(), 'Character E', 'the disabled option is skipped');
    await page.press('ArrowUp');
    assert.equal(await activeOption(), 'Character C');
    await page.press('End');
    assert.equal(await activeOption(), 'Dan');
    await page.press('c');
    assert.equal(await activeOption(), 'Character C', 'type-ahead');
    await page.press('c');
    assert.equal(await activeOption(), 'Character E', 'the same letter again cycles');
    await page.press('Enter');
    assert.equal(await isOpen(list), false, 'Enter closes the list');
    assert.equal(await shown(), 'Character E');
    assert.equal(await page.js(`document.activeElement === document.querySelector('${trigger}')`), true, 'focus is still on the button');
    await page.press('ArrowDown');
    await page.waitFor(`document.querySelector('${list}').matches(':popover-open')`, 'the list to open');
    assert.equal(await activeOption(), 'Character E', 'it opens on the chosen option');
    await page.press('Escape');
    assert.equal(await isOpen(list), false, 'Escape closes the list');
    assert.equal(await shown(), 'Character E', 'Escape keeps the value');
    assert.equal(await page.js(`document.activeElement === document.querySelector('${trigger}')`), true);
    if (insideDialog) assert.equal(await dialogOpen(), true, 'the first Escape does not close the dialog');
    await page.press('ArrowDown');
    await page.waitFor(`document.querySelector('${list}').matches(':popover-open')`, 'the list to open');
    await page.click(`${list} [data-value="dan"]`);
    assert.equal(await isOpen(list), false, 'a click on an option closes the list');
    assert.equal(await shown(), 'Dan', 'a click on an option chooses it');
    assert.equal(await page.js(`document.getElementById('lab').dataset.${insideDialog ? 'dialogSelect' : 'pageSelect'}`), 'dan');
  }

  check('select, on the page: open, arrows, Home, End, type-ahead, choose, Escape, mouse', async () => {
    await selectSuite('#fields', false);
  });

  for (const name of THEMES) {
    check(`${name}: every owned component draws a 2 pixel focus ring in the focus colour on a real Tab walk`, async () => {
      await theme(name);
      const focus = await colourOf('--focus');
      await page.focus('#start');
      const seen = new Set();
      const bad = [];
      for (let step = 0; step < 80; step += 1) {
        await page.press('Tab');
        const stop = await page.js(`(() => { const a = document.activeElement; const s = getComputedStyle(a);
          const owner = a.closest('[data-ui]');
          return { id: a.id, hasFocus: document.hasFocus(), kind: owner ? owner.dataset.ui : null, label: a.getAttribute('aria-label') || a.textContent.trim().slice(0, 24),
            style: s.outlineStyle, width: s.outlineWidth, colour: s.outlineColor }; })()`);
        assert.equal(stop.hasFocus, true, `step ${step}: the page still has focus`);
        if (stop.id === 'end') break;
        if (stop.kind === null) continue;
        seen.add(stop.kind);
        if (stop.style !== 'solid' || parseFloat(stop.width) < 2 || stop.colour !== focus) bad.push(`${stop.kind} "${stop.label}": ${stop.style} ${stop.width} ${stop.colour}`);
      }
      assert.deepEqual(bad, []);
      assert.deepEqual([...seen].sort(), ['button', 'choice', 'disclosure', 'icon-button', 'input', 'menu-button', 'select', 'switch', 'tab', 'textarea'],
        'the walk reached a button, choice, icon button, input, textarea, select, menu button, switch, tab and disclosure');
    });
  }

  check('the dialog opens modal, traps focus, and its rings, menu and select work above it', async () => {
    await theme('light');
    await page.click('#open-dialog');
    assert.equal(await dialogOpen(), true);
    assert.equal(await page.js(`document.getElementById('the-dialog').matches(':modal')`), true);
    const focus = await colourOf('--focus');
    const stops = [];
    await page.js(`document.getElementById('dialog-name').focus()`);
    for (let step = 0; step < 4; step += 1) {
      const stop = await page.js(`(() => { const a = document.activeElement; const s = getComputedStyle(a);
        return { kind: a.closest('[data-ui]').dataset.ui, inside: document.getElementById('the-dialog').contains(a), hasFocus: document.hasFocus(), style: s.outlineStyle, width: s.outlineWidth, colour: s.outlineColor }; })()`);
      assert.equal(stop.inside, true);
      assert.equal(stop.hasFocus, true);
      assert.deepEqual([stop.style, stop.width, stop.colour], ['solid', '2px', focus], stop.kind);
      stops.push(stop.kind);
      if (step < 3) await page.press('Tab');
    }
    assert.deepEqual(stops, ['input', 'select', 'menu-button', 'button']);
    await menuSuite('#the-dialog', true);
    await selectSuite('#the-dialog', true);
    await page.focus('#the-dialog [data-ui="select"]');
    await page.press('Escape');
    assert.equal(await dialogOpen(), false, 'Escape with nothing open closes the dialog');
    assert.equal(await page.active(), 'open-dialog', 'focus returns to the button that opened it');
  });

  check('a choice is a real radio or checkbox: arrows move a radio group, Space toggles a checkbox, and its mark shows the state', async () => {
    await theme('light');
    await page.focus('#choices [name="lab-engine"][value="claude"]');
    await page.press('ArrowDown');
    await page.waitFor(`document.getElementById('choice-now').textContent === 'codex'`, 'the arrow key to choose Codex');
    assert.notEqual(await page.js(`getComputedStyle(document.querySelector('#choices [name="lab-engine"][value="codex"]')).backgroundColor`),
      await page.js(`getComputedStyle(document.querySelector('#choices [name="lab-engine"][value="claude"]')).backgroundColor`),
      'the chosen radio is drawn differently from the other')
    await page.focus('#choices [name="lab-copy"]');
    await page.press(' ');
    await page.waitFor(`document.getElementById('choice-now').textContent === 'codex copy'`, 'Space to tick the box');
    assert.equal(await page.js(`document.querySelector('#choices [name="lab-copy"]').checked`), true);
  });

  check('a notice carries its role and does not take focus', async () => {
    assert.equal(await page.js(`document.querySelector('#notices [data-tone="danger"]').getAttribute('role')`), 'alert');
    assert.equal(await page.js(`document.querySelectorAll('#notices [tabindex]').length`), 0);
  });

  check('a toast is a status region, and Dismiss works from the keyboard', async () => {
    assert.equal(await page.js(`document.querySelector('#toasts [role="status"]').textContent`), 'Accepted. The ticket is Done.');
    await page.focus('#toasts [aria-label="Dismiss"]');
    await page.press('Enter');
    await page.waitFor(`document.getElementById('toast-gone')`, 'the toast to go');
    assert.equal(await page.js(`document.querySelector('#toasts [role="status"]')`), null);
  });

  check('the page policy saw no violation, no style element, no style attribute and no console error', async () => {
    const seen = await page.js(`window.__policy`);
    assert.deepEqual(seen, { violations: [], styles: [], styleAttributes: [] });
    assert.deepEqual(log.errors, []);
  });

  return list;
}
