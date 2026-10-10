// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Mutation controls, each named by the test it turns red:
//   - the drawer portrait read out to assistive technology ... "the drawer portrait is the agent's figure…"
//   - the badge on a fixed colour rather than the Crew one ..... "the badge is the agent's initial…"
//   - the initial taken as the first UTF-16 unit .............. "the initial is the first character a reader sees…"
//   - the segmenter built when the module loads ............... "without Intl.Segmenter the module loads…"
//   - no page canvas asked before the offscreen renderer ...... "a page with 3D switched off gets no figure…"
//   - a failed offscreen renderer tried again on every paint .. "without OffscreenCanvas the figure is unavailable…"
//   - the page asked about 3D before the picture cache ........ "a remembered portrait still paints…"
//   - the figure taken out of the scene only after a paint .... "a render or a copy that throws…"
//   - one failed look turning every later look into a badge ... "after one look fails to paint…"

import assert from 'node:assert/strict';
import { resolve } from 'node:path';
import test, { after, before } from 'node:test';
import { act, createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';

/** The portrait module draws with `globalThis.portraitRenderer` in place of three's WebGL renderer. */
const studioRenderer = {
  name: 'portrait-studio-renderer',
  enforce: 'pre',
  transform(code, id) {
    if (!id.endsWith('/src/renderer/figurePortrait.ts')) return null;
    assert.ok(code.includes("} from 'three';"), 'the portrait module no longer imports three as this test expects');
    return code.replace("} from 'three';", "} from 'virtual:portrait-three';");
  },
  resolveId: (id) => (id === 'virtual:portrait-three' ? '\0portrait-three' : null),
  load: (id) => (id === '\0portrait-three'
    ? "export * from 'three'; export const WebGLRenderer = globalThis.portraitRenderer;" : null),
};

let server;
let AgentDetail;
let portrait;
let lookOf;
before(async () => {
  server = await createServer({ root: resolve(import.meta.dirname, '..'), plugins: [studioRenderer],
    server: { middlewareMode: true, hmr: false }, appType: 'custom', logLevel: 'silent' });
  ({ AgentDetail } = await server.ssrLoadModule('/src/renderer/AgentDetail.tsx'));
  portrait = await server.ssrLoadModule('/src/renderer/Portrait.tsx');
  ({ lookOf } = await server.ssrLoadModule('/src/renderer/appearance.ts'));
});
after(async () => { await server?.close(); });

const row = { id: '01ARZ3NDEKTSV4RRFFQ69G5FAV', name: 'Jonas', role: 'frontend builder', character: 'h', accent: 'sky',
  provider: 'codex', status: 'waiting', mode: 'managed' };

test('the drawer portrait is the agent\'s figure, hidden from assistive technology beside its name', () => {
  const html = renderToStaticMarkup(createElement(AgentDetail, { row }));
  const header = html.match(/<header class="agent-detail-head">([\s\S]*?)<\/header>/)?.[1] ?? '';
  assert.match(header, /^<canvas class="agent-portrait agent-detail-portrait portrait-figure" aria-hidden="true" data-portrait="figure" data-skin="#e8b98f" data-hair="#6b3b2a" data-shirt="#397ca9"><\/canvas><div class="agent-detail-identity"><p class="agent-detail-kicker">Selected agent<\/p><div class="agent-detail-name"><h2 id="agent-detail-title" tabindex="-1">Jonas<\/h2>/);
  assert.doesNotMatch(html, /<svg|crispEdges/, 'the pixel portrait is still drawn');
});

test('the badge is the agent\'s initial on its Crew colour', () => {
  const badge = (look, name) => renderToStaticMarkup(createElement(portrait.PortraitBadge,
    { look, name, className: 'agent-portrait' }));
  assert.equal(badge(lookOf(row), 'jonas'), '<span class="agent-portrait portrait-badge" aria-hidden="true" data-portrait="badge" '
    + 'data-skin="#e8b98f" data-hair="#6b3b2a" data-shirt="#397ca9" style="--agent:#397ca9">J</span>');
  assert.match(badge(lookOf({ ...row, accent: 'rose' }), 'Ada'), /data-shirt="#b94e79" style="--agent:#b94e79">A<\/span>$/);
});

test('the initial is the first character a reader sees in the name, and nothing for no name', () => {
  for (const [name, initial] of [
    ['jonas', 'J'], ['  émile ', 'É'], ['e\u0301va', 'E\u0301'], ['🦊 Fox', '🦊'],
    ['', ''], ['   ', ''], [undefined, ''], [42, ''],
  ]) assert.equal(portrait.initialOf(name), initial, JSON.stringify(name));
});

/** The module evaluated afresh, so its once-per-page state starts empty. */
async function fresh(path) {
  server.moduleGraph.invalidateAll();
  return server.ssrLoadModule(path);
}

test('without Intl.Segmenter the module loads, and the initial is the first code point', async () => {
  const segmenter = Object.getOwnPropertyDescriptor(Intl, 'Segmenter');
  delete Intl.Segmenter;
  try {
    const { initialOf } = await fresh('/src/renderer/Portrait.tsx');
    for (const [name, initial] of [['jonas', 'J'], ['  émile ', 'É'], ['🦊 Fox', '🦊'], ['   ', ''], [undefined, '']]) {
      assert.equal(initialOf(name), initial, JSON.stringify(name));
    }
  } finally {
    Object.defineProperty(Intl, 'Segmenter', segmenter);
    server.moduleGraph.invalidateAll();
  }
});

/**
 * Just enough of a page for React to mount a portrait, set as the global
 * `document` and `window`. Every canvas answers `webgl2` as `webgl` says and
 * keeps the pictures put into its 2D context; `probes` counts the WebGL asks.
 */
function fakePage({ webgl = true } = {}) {
  const page = { probes: 0 };
  class Element {
    nodeType = 1;
    ownerDocument = page.document;
    childNodes = [];
    parentNode = null;
    attributes = new Map();
    style = { setProperty(name, value) { this[name] = value; } };
    painted = [];
    constructor(tag) { this.nodeName = this.tagName = tag.toUpperCase(); }
    get firstChild() { return this.childNodes[0] ?? null; }
    appendChild(child) { return this.insertBefore(child, null); }
    insertBefore(child, next) {
      child.parentNode?.removeChild(child);
      const at = this.childNodes.indexOf(next);
      this.childNodes.splice(at === -1 ? this.childNodes.length : at, 0, child);
      child.parentNode = this;
      return child;
    }
    removeChild(child) {
      this.childNodes.splice(this.childNodes.indexOf(child), 1);
      child.parentNode = null;
      return child;
    }
    setAttribute(name, value) { this.attributes.set(name, String(value)); }
    addEventListener() {}
    getContext(kind) {
      if (kind === '2d') return { putImageData: (picture) => this.painted.push(picture) };
      page.probes += 1;
      return kind === 'webgl2' && webgl ? { getExtension: () => null } : null;
    }
  }
  page.document = { nodeType: 9, addEventListener() {}, createElement: (tag) => new Element(tag) };
  page.document.defaultView = { document: page.document, HTMLIFrameElement: class {} };
  globalThis.document = page.document;
  globalThis.window = page.document.defaultView;
  return page;
}

/**
 * The offscreen renderer the portrait module opens, faked. `fail` names the
 * step of a paint that throws, `lost` loses its context and `refuse` makes the
 * next one fail to open; the scene it last drew, each figure geometry it drew
 * and the ones freed since are kept for the test to read.
 */
function fakeStudio() {
  const studio = { fail: null, lost: false, refuse: false, scene: null, drawn: [], freed: new Set() };
  globalThis.OffscreenCanvas = class {
    getContext() {
      return {
        clearRect() {},
        drawImage() { if (studio.fail === 'copy') throw new Error('the copy failed'); },
        getImageData: (x, y, width, height) => ({ width, height }),
      };
    }
  };
  globalThis.portraitRenderer = class {
    constructor() { if (studio.refuse) throw new Error('no WebGL context'); }
    setClearColor() {}
    setSize() {}
    dispose() {}
    getContext() { return { isContextLost: () => studio.lost }; }
    render(scene) {
      studio.scene = scene;
      scene.traverse((node) => {
        if (!node.isMesh) return;
        studio.drawn.push(node.geometry);
        node.geometry.addEventListener('dispose', () => studio.freed.add(node.geometry));
      });
      if (studio.fail === 'render') throw new Error('the render failed');
    }
  };
  return studio;
}

function clearFakes() {
  for (const name of ['document', 'window', 'OffscreenCanvas', 'portraitRenderer']) delete globalThis[name];
}

/** An OffscreenCanvas that counts how often anything builds one, and refuses every build. */
function countingOffscreen() {
  const built = { count: 0 };
  globalThis.OffscreenCanvas = class {
    constructor() {
      built.count += 1;
      throw new Error('no offscreen canvas here');
    }
  };
  return built;
}

test('a page with 3D switched off gets no figure and opens no offscreen renderer', async () => {
  const page = fakePage({ webgl: false });
  const built = countingOffscreen();
  try {
    const { paintFigurePortrait } = await fresh('/src/renderer/figurePortrait.ts');
    assert.equal(paintFigurePortrait(page.document.createElement('canvas'), lookOf(row), 'headphones'), false);
    assert.equal(built.count, 0, 'an offscreen renderer was opened on a page that refuses WebGL');
    assert.equal(page.probes, 1);
  } finally {
    clearFakes();
  }
});

test('without OffscreenCanvas the figure is unavailable, and later paints do not try again', async () => {
  assert.equal(typeof globalThis.OffscreenCanvas, 'undefined');
  const page = fakePage();
  try {
    const { paintFigurePortrait } = await fresh('/src/renderer/figurePortrait.ts');
    assert.equal(paintFigurePortrait(page.document.createElement('canvas'), lookOf(row), 'headphones'), false);
    const built = countingOffscreen();
    assert.equal(paintFigurePortrait(page.document.createElement('canvas'), lookOf({ ...row, accent: 'rose' }), null),
      false);
    assert.equal(built.count, 0, 'a later paint tried the offscreen renderer again');
    assert.equal(page.probes, 1, 'the page was asked about WebGL more than once');
  } finally {
    clearFakes();
  }
});

test('a remembered portrait still paints after a lost context whose renderer will not reopen', async () => {
  const page = fakePage();
  const studio = fakeStudio();
  try {
    const { paintFigurePortrait } = await fresh('/src/renderer/figurePortrait.ts');
    const first = page.document.createElement('canvas');
    assert.equal(paintFigurePortrait(first, lookOf(row), 'headphones'), true);
    studio.lost = true;
    studio.refuse = true;
    assert.equal(paintFigurePortrait(page.document.createElement('canvas'), lookOf({ ...row, accent: 'rose' }), null),
      false, 'a new look painted with no renderer');
    const again = page.document.createElement('canvas');
    assert.equal(paintFigurePortrait(again, lookOf(row), 'headphones'), true, 'the remembered look fell back to the badge');
    assert.equal(again.painted.length, 1);
    assert.equal(again.painted[0], first.painted[0], 'the remembered look was drawn afresh');
  } finally {
    clearFakes();
  }
});

test('a render or a copy that throws takes the figure back out of the shared scene and frees its geometries', async () => {
  for (const step of ['render', 'copy']) {
    const page = fakePage();
    const studio = fakeStudio();
    const scenery = () => studio.scene.children.map((child) => child.type);
    try {
      const { paintFigurePortrait } = await fresh('/src/renderer/figurePortrait.ts');
      studio.fail = step;
      assert.equal(paintFigurePortrait(page.document.createElement('canvas'), lookOf(row), 'headphones'), false);
      assert.deepEqual(scenery(), ['HemisphereLight', 'DirectionalLight'], `a failed ${step} left the figure in the scene`);
      assert.equal(studio.drawn.length, 10, 'five limbs, each a shape and an edge');
      assert.equal(studio.freed.size, 10, `a failed ${step} kept the figure's geometries`);
      studio.fail = null;
      assert.equal(paintFigurePortrait(page.document.createElement('canvas'), lookOf({ ...row, accent: 'rose' }), null),
        true, `after a failed ${step}, the next look did not paint`);
      assert.deepEqual(scenery(), ['HemisphereLight', 'DirectionalLight']);
      assert.equal(studio.freed.size, 20);
    } finally {
      clearFakes();
    }
  }
});

/** Let React work until `settled()` holds; a portrait that never settles fails rather than hangs. */
async function until(settled) {
  for (let turn = 0; !settled(); turn += 1) {
    assert.ok(turn < 200, 'the portrait never settled');
    await act(() => new Promise((resolve) => { setTimeout(resolve, 10); }));
  }
}

test('after one look fails to paint, the next look paints its figure', async () => {
  const page = fakePage();
  const studio = fakeStudio();
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  try {
    await fresh('/src/renderer/figurePortrait.ts');
    const { Portrait } = await server.ssrLoadModule('/src/renderer/Portrait.tsx');
    const { createRoot } = await import('react-dom/client');
    const host = page.document.createElement('div');
    const root = createRoot(host);
    const show = (look) => act(() => root.render(createElement(Portrait,
      { look, name: 'Jonas', className: 'agent-portrait' })));
    const shown = () => host.firstChild?.attributes.get('data-portrait');
    studio.fail = 'render';
    await show(lookOf(row));
    await until(() => shown() === 'badge');
    studio.fail = null;
    await show(lookOf({ ...row, accent: 'rose' }));
    await until(() => shown() === 'badge' || host.firstChild.painted.length > 0);
    assert.equal(shown(), 'figure', 'the look after a failed one fell back to the badge');
    assert.equal(host.firstChild.painted.length, 1);
    await act(() => root.unmount());
  } finally {
    clearFakes();
    delete globalThis.IS_REACT_ACT_ENVIRONMENT;
  }
});
