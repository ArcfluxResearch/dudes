// What a check does to the page: evaluate in it, press real keys and real mouse buttons, and wait for the result.

const KEYS = {
  Enter: { code: 'Enter', vk: 13, text: '\r' }, Escape: { code: 'Escape', vk: 27 }, Tab: { code: 'Tab', vk: 9 },
  ArrowDown: { code: 'ArrowDown', vk: 40 }, ArrowUp: { code: 'ArrowUp', vk: 38 }, ArrowLeft: { code: 'ArrowLeft', vk: 37 },
  ArrowRight: { code: 'ArrowRight', vk: 39 }, Home: { code: 'Home', vk: 36 }, End: { code: 'End', vk: 35 },
  ' ': { code: 'Space', vk: 32, text: ' ' },
};

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

export function pageOf(cdp) {
  async function js(expression) {
    const { result, exceptionDetails } = await cdp.send('Runtime.evaluate', { expression, returnByValue: true, awaitPromise: true });
    if (exceptionDetails) throw new Error(`${exceptionDetails.exception?.description ?? exceptionDetails.text} in: ${expression.slice(0, 160)}`);
    return result.value;
  }

  const settle = () => js('new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(done, 30))))');

  async function press(key, { shift = false } = {}) {
    const known = KEYS[key] ?? { code: `Key${key.toUpperCase()}`, vk: key.toUpperCase().charCodeAt(0), text: key };
    const base = { key, code: known.code, windowsVirtualKeyCode: known.vk, modifiers: shift ? 8 : 0 };
    await cdp.send('Input.dispatchKeyEvent', { type: known.text === undefined ? 'rawKeyDown' : 'keyDown', ...base, text: known.text });
    await cdp.send('Input.dispatchKeyEvent', { type: 'keyUp', ...base });
    await settle();
  }

  async function waitFor(expression, what, ms = 3000) {
    for (let waited = 0; waited < ms; waited += 25) {
      if (await js(`Boolean(${expression})`)) return;
      await sleep(25);
    }
    throw new Error(`timed out waiting for ${what}`);
  }

  async function centre(selector) {
    const box = await js(`(() => { const el = document.querySelector(${JSON.stringify(selector)}); if (!el) return null;
      el.scrollIntoView({ block: 'center' }); const r = el.getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`);
    if (box === null) throw new Error(`no element for ${selector}`);
    return box;
  }

  async function move(x, y) {
    await cdp.send('Input.dispatchMouseEvent', { type: 'mouseMoved', x, y });
  }

  async function click(selector) {
    const { x, y } = await centre(selector);
    await move(x, y);
    await cdp.send('Input.dispatchMouseEvent', { type: 'mousePressed', x, y, button: 'left', clickCount: 1 });
    await cdp.send('Input.dispatchMouseEvent', { type: 'mouseReleased', x, y, button: 'left', clickCount: 1 });
    await settle();
  }

  const focus = async (selector) => { await js(`document.querySelector(${JSON.stringify(selector)}).focus()`); await settle(); };
  const active = () => js(`(() => { const a = document.activeElement; return a ? (a.id || a.getAttribute('aria-label') || a.textContent.trim().slice(0, 40)) : null; })()`);

  return { js, settle, press, waitFor, centre, move, click, focus, active, sleep, send: cdp.send };
}
