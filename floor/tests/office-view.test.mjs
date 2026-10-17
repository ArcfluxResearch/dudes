// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The office camera's pure decisions, what is laid over the canvas, the
// fallback when a browser cannot draw the office, and the status badges.

import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import { createElement } from 'react';
import { renderToStaticMarkup } from 'react-dom/server';
import { createServer } from 'vite';
import { resolve } from 'node:path';

import { BADGE_COLOURS, discPixels, drawBadge } from '../src/renderer/office3d/badges.ts';
import { OFFICE_FALLBACK, startRenderer } from '../src/renderer/office3d/support.ts';
import { watchCover } from '../src/renderer/office3d/cover.ts';
import { ZOOM_LIMITS, coveredInsets, fitHalfHeight, followTarget, frustum, keepsWhole, layTags, narrowBand, onCanvas, spreadTags } from '../src/renderer/office3d/view.ts';
import { themeTokens } from './helpers/theme-colour.mjs';

async function withOffice(run) {
  const server = await createServer({ root: resolve(import.meta.dirname, '..'),
    cacheDir: resolve(import.meta.dirname, '../node_modules/.vite-office-view'), server: { middlewareMode: true, hmr: false }, appType: 'custom' });
  try {
    await run(await server.ssrLoadModule('/src/renderer/OfficeCanvas.tsx'), server);
  } finally {
    await server.close();
  }
}

function contrast(foreground, background) {
  const channel = (value) => {
    const c = value / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  const luminance = (hex) => {
    const n = Number.parseInt(hex.slice(1), 16);
    return 0.2126 * channel((n >> 16) & 255) + 0.7152 * channel((n >> 8) & 255) + 0.0722 * channel(n & 255);
  };
  const [lighter, darker] = [luminance(foreground), luminance(background)].sort((a, b) => b - a);
  return (lighter + 0.05) / (darker + 0.05);
}

test('FOLLOW_AT_FIT mutant: the view leans toward the selected agent only as it zooms in', () => {
  assert.deepEqual(followTarget(null, ZOOM_LIMITS.max), { x: 0, z: 0 });
  assert.deepEqual(followTarget({ x: 6, z: -4 }, 1), { x: 0, z: 0 });
  assert.deepEqual(followTarget({ x: 6, z: -4 }, 0.8), { x: 0, z: 0 });
  assert.deepEqual(followTarget({ x: 6, z: -4 }, 1.8), { x: 3, z: -2 });
  assert.deepEqual(followTarget({ x: 6, z: -4 }, ZOOM_LIMITS.max), { x: 6, z: -4 });
});

test('the fit holds the whole floor in a wide or a tall canvas, the same from every side', () => {
  const wide = fitHalfHeight(20, 14, 16 / 9);
  const tall = fitHalfHeight(20, 14, 3 / 4);
  assert.equal(Number(wide.toFixed(3)), 7.632);
  assert.equal(Number(tall.toFixed(3)), 14.973);
  assert.ok(tall * 0.75 >= Math.hypot(20, 14) / 2 * 0.92 - 1e-9, 'a tall canvas still fits the diagonal across');
});

test('TAG_OVERLAP mutant: crowded name tags are lifted apart, the nearest keeps its place, and none is dropped', () => {
  const tag = (id, x, y) => ({ id, x, y, full: { width: 80, height: 20 }, compact: { width: 80, height: 20 }, whole: false });
  const bottoms = (tags) => [...spreadTags(tags).entries()].map(([id, at]) => [id, at.bottom]).sort();
  assert.deepEqual(bottoms([tag('far', 100, 290), tag('near', 110, 300), tag('apart', 400, 300), tag('nearest', 100, 310)]),
    [['apart', 300], ['far', 266], ['near', 288], ['nearest', 310]]);
  assert.deepEqual(bottoms([tag('one', 0, 50), tag('two', 200, 50)]), [['one', 50], ['two', 50]], 'tags that do not touch stay put');
});

test('CROWD_KEEPS_WHOLE and SELECTED_SHORTENED mutants: tags crowded at their figures show the name alone, so a sofa of three reads side by side', () => {
  const tag = (id, x, y, whole = false) => ({ id, x, y, full: { width: 96, height: 32 }, compact: { width: 30, height: 18 }, whole });
  const sofa = spreadTags([tag('kay', 400, 500), tag('mo', 432, 503), tag('sam', 464, 506), tag('oz', 700, 500)]);
  assert.deepEqual(Object.fromEntries(sofa), {
    kay: { bottom: 500, compact: true }, mo: { bottom: 503, compact: true }, sam: { bottom: 506, compact: true },
    oz: { bottom: 500, compact: false },
  }, 'each name sits over its own figure, and a tag with room keeps its engine and activity');
  const selected = spreadTags([tag('kay', 400, 500, true), tag('mo', 432, 503)]);
  assert.deepEqual(Object.fromEntries(selected), { kay: { bottom: 483, compact: false }, mo: { bottom: 503, compact: true } },
    'the selected tag stays whole, and rises clear of the name beside it');
});

test('FOCUSED_SHORTENED and SELECTED_SHORTENED_AGAIN mutants: a crowded tag stays whole while its agent is selected or its Inspect button has the keyboard', () => {
  const page = { activeElement: null };
  const inspect = { hidden: false, ownerDocument: page };
  const view = { hidden: false, ownerDocument: page };
  const tag = (selected) => ({ dataset: { selected: String(selected) } });
  const whole = () => ({ none: keepsWhole(tag(false), inspect), selected: keepsWhole(tag(true), inspect), leaving: keepsWhole(tag(false), undefined) });
  assert.deepEqual(whole(), { none: false, selected: true, leaving: false });
  page.activeElement = inspect;
  assert.deepEqual(whole(), { none: true, selected: true, leaving: false }, 'its own Inspect button has the keyboard');
  page.activeElement = view;
  assert.deepEqual(whole(), { none: false, selected: true, leaving: false }, 'the keyboard moved on, to a view button');
  const sofa = (focused) => Object.fromEntries(spreadTags([
    { id: 'kay', x: 400, y: 500, full: { width: 96, height: 32 }, compact: { width: 30, height: 18 }, whole: focused },
    { id: 'mo', x: 432, y: 503, full: { width: 96, height: 32 }, compact: { width: 30, height: 18 }, whole: false },
  ]));
  assert.deepEqual(sofa(keepsWhole(tag(false), (page.activeElement = inspect))), { kay: { bottom: 483, compact: false }, mo: { bottom: 503, compact: true } },
    'the focused tag shows whole and rises clear of the name beside it');
});

test('COMPACT_UNWRITTEN and COMPACT_UNSTYLED mutants: a crowded tag says data-compact, and the stylesheet then shows its name alone', async () => {
  const tag = (id, x, y) => ({ id, x, y, full: { width: 96, height: 32 }, compact: { width: 30, height: 18 }, whole: false,
    element: { dataset: {}, style: { left: '', top: '' } } });
  const tags = [tag('kay', 400, 500), tag('mo', 432, 503), tag('oz', 700, 500)];
  layTags(tags);
  assert.deepEqual(tags.map(({ id, element }) => [id, element.dataset.compact, element.style.left, element.style.top]), [
    ['kay', 'true', '400px', '500px'], ['mo', 'true', '432px', '503px'], ['oz', 'false', '700px', '500px'],
  ], 'the two on the sofa show their names side by side, and the tag with room keeps its engine');
  const css = await readFile(resolve(import.meta.dirname, '../src/style.css'), 'utf8');
  const rule = /\.office-tag\[data-compact='true'\] \.office-tag-engine,\s*\.office-tag\[data-compact='true'\] small\s*\{([^}]*)\}/.exec(css)?.[1] ?? '';
  assert.match(rule, /display:\s*none;/, 'a compact tag hides the engine and the activity line');
});

test('GOALS_UNCOUNTED and GOAL_SLOTS_MISCOUNTED mutants: the board button counts the open goals past each board\'s goal notes', async () => {
  const rows = (open, abandoned = 0) => [
    ...Array.from({ length: open }, (_, k) => ({ id: `01JBQ8XK2M3N4P5Q6R7S8T9V${String(k).padStart(2, '0')}`, goal: { status: 'in_progress' } })),
    ...Array.from({ length: abandoned }, (_, k) => ({ id: `01JBQ8XK2M3N4P5Q6R7S8T9W${String(k).padStart(2, '0')}`, goal: { status: 'abandoned' } })),
  ];
  await withOffice(({ goalsPastBoard }) => {
    assert.deepEqual({
      hq: goalsPastBoard(rows(8, 2), 'hq'),
      workshop: goalsPastBoard(rows(8, 2), 'workshop'),
      studio: goalsPastBoard(rows(11), 'studio'),
      noGoals: goalsPastBoard([{ id: '01JBQ8XK2M3N4P5Q6R7S8T9VWX' }], 'hq'),
    }, { hq: 2, workshop: 0, studio: 3, noGoals: 0 }, 'HQ draws six goal notes and the others eight; an abandoned goal is not on the board');
  });
});

test('WEBGL_FALLBACK mutant: a browser that refuses WebGL gets no renderer and the short line instead', () => {
  assert.deepEqual(startRenderer(() => { throw new Error('Error creating WebGL context.'); }), { support: 'unavailable', renderer: null });
  const renderer = { draws: true };
  assert.deepEqual(startRenderer(() => renderer), { support: 'ready', renderer });
  assert.match(OFFICE_FALLBACK.unavailable, /Crew still lists every agent/);
  assert.match(OFFICE_FALLBACK.lost, /Retry/);
});

test('FULL_HEARD_DROPPED and FULL_FLAG_UNCONFIRMED mutants: the office renders its loading line, its canvas and a quiet live region before the renderer starts', async () => {
  await withOffice(({ OfficeCanvas }) => {
    const render = (epochs) => renderToStaticMarkup(createElement(OfficeCanvas, {
      floor: { phase: 'live', stale: false, ...epochs, agents: [], cards: [], settings: null, onboarding: null },
      selectedId: null, onSelectAgent() {}, onOpenTasks() {}, onAddAgent() {},
    }));
    const html = render({ epoch: 1, snapshotEpoch: 1 });
    assert.match(html, /<div class="office" data-office="loading" data-layout="studio" data-full="false"><canvas class="office-canvas">/);
    assert.match(render({ epoch: 2, snapshotEpoch: 1 }), /<div class="office" data-office="loading" data-layout="studio"><canvas class="office-canvas">/,
      'rows no snapshot confirms say nothing of the crew');
    assert.match(html, /<p role="status">Loading the office\.<\/p>/);
    assert.doesNotMatch(html, /office-failure|agent-select|office-zoom/, 'nothing is laid over a drawing that does not exist yet');
    assert.match(html, /<p class="office-full-heard" role="status"><\/p>/, 'the office-full live region is there, and quiet, from the start');
  });
});

test('OFFICE_FIXED, FULL_LINE_DROPPED, FULL_LINE_DRAWN_ONLY and FULL_LINE_ROLE mutants: the office grows with the crew, says when even HQ is full, and follows the pin', async () => {
  const crew = (count) => Array.from({ length: count }, (_, k) => ({ id: `01JBQ8XK2M3N4P5Q6R7S8T9V${String(k).padStart(2, '0')}`, status: 'running' }));
  await withOffice(async ({ OfficeCanvas }, server) => {
    const { setOfficePin } = await server.ssrLoadModule('/src/renderer/view.ts');
    const render = (count) => renderToStaticMarkup(createElement(OfficeCanvas, {
      floor: { phase: 'live', stale: false, epoch: 1, snapshotEpoch: 1, agents: crew(count), cards: [], settings: null, onboarding: null },
      selectedId: null, onSelectAgent() {}, onOpenTasks() {}, onAddAgent() {},
    }));
    const office = (html) => html.match(/<div class="office" data-office="loading" data-layout="(\w+)" data-full="(\w+)">/)?.slice(1);
    assert.deepEqual(office(render(8)), ['studio', 'false']);
    assert.deepEqual(office(render(9)), ['workshop', 'false']);
    assert.deepEqual(office(render(13)), ['hq', 'false']);
    const full = render(17);
    assert.deepEqual(office(full), ['hq', 'true']);
    // The line shows while the office loads, as `data-full` does, and leaves only with the drawing, for the fallback.
    assert.match(full, /<p class="office-full">The office is full\. HQ has 16 desks for 17 agents\. 1 agent has no desk\.<\/p>/);
    assert.match(full, /<p class="office-full-heard" role="status"><\/p>/, 'the live region renders empty; only a later change of the line speaks there');
    assert.equal(full.match(/<li data-agent-id=/g)?.length, 17, 'the mirror list carries every agent');
    setOfficePin('studio');
    const pinned = render(12);
    assert.deepEqual(office(pinned), ['studio', 'true']);
    assert.match(pinned, /Studio is pinned in Settings\. It has 8 desks for 12 agents\. 4 agents have no desk\./);
    setOfficePin('hq');
    assert.deepEqual(office(render(3)), ['hq', 'false']);
    setOfficePin(null);
    assert.deepEqual(office(render(3)), ['studio', 'false']);
  });
});

test('NAME_UNISOLATED mutant: a name written right to left cannot reorder the engine after it', async () => {
  await withOffice(({ NameTag }) => {
    const id = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
    assert.equal(renderToStaticMarkup(createElement(NameTag, { id, row: { id, name: '\u202eAda', provider: 'claude' } })),
      '<bdi>\u202eAda</bdi><span class="office-tag-engine"> · claude</span>');
    assert.equal(renderToStaticMarkup(createElement(NameTag, { id, row: { id, name: '' } })), `<bdi>${id}</bdi>`);
  });
});

test('NAME_UNCAPPED mutant: a very long name is cut by the stylesheet, while the markup keeps it whole', async () => {
  const name = 'Margaret Hamilton the Apollo flight software lead and long-running reviewer';
  await withOffice(({ NameTag }) => {
    const id = '01ARZ3NDEKTSV4RRFFQ69G5FAV';
    assert.equal(renderToStaticMarkup(createElement(NameTag, { id, row: { id, name, provider: 'claude' } })),
      `<bdi>${name}</bdi><span class="office-tag-engine"> · claude</span>`);
  });
  const css = await readFile(resolve(import.meta.dirname, '../src/style.css'), 'utf8');
  const rule = /\.office-tag bdi\s*\{([^}]*)\}/.exec(css)?.[1] ?? '';
  assert.match(rule, /max-width:\s*16ch;/);
  assert.match(rule, /overflow:\s*hidden;/);
  assert.match(rule, /text-overflow:\s*ellipsis;/);
  assert.doesNotMatch(css, /\.office-tag-engine\s*\{[^}]*overflow/, 'the engine suffix is not cut');
});

test('PANEL_INSETS and OVERLAY_EDGES_REDUCED mutants: a tall panel covers the edge its middle is on, and a third of the office stays uncovered', () => {
  const office = { left: 0, top: 65, right: 1440, bottom: 1000 };
  const crew = { left: 10, top: 75, right: 240, bottom: 990 };
  const drawer = { left: 1015, top: 340, right: 1430, bottom: 990 };
  const receipts = { left: 1015, top: 75, right: 1430, bottom: 330 };
  assert.deepEqual(coveredInsets(office, [crew, drawer, receipts]), { left: 240, right: 425 });
  assert.deepEqual(coveredInsets(office, [receipts]), { left: 0, right: 0 }, 'a short panel covers no edge');
  assert.deepEqual(coveredInsets(office, []), { left: 0, right: 0 });
  assert.deepEqual(coveredInsets(office, [{ left: 0, top: 65, right: 700, bottom: 1000 }, { left: 740, top: 65, right: 1440, bottom: 1000 }]),
    { left: 480, right: 480 }, 'panels that would cover everything leave the middle third');
  assert.deepEqual(coveredInsets(office, [{ left: 0, top: 65, right: 700, bottom: 1000 }, { left: 740, top: 65, right: 1440, bottom: 1000 }], 0),
    { left: 700, right: 700 }, 'what is laid over the office gets the panels\' real edges');
});

test('BAND_IGNORED, BAND_NO_HYSTERESIS and HIDDEN_BAND_NARROW mutants: the office\'s lines move to the top bar under 12rem of free band and come back only above 13rem', () => {
  const edges = (band) => ({ left: 400, right: 1000 - 400 - band });
  let narrow = false;
  const crossing = [250, 192, 191, 200, 207, 208, 250].map((band) => (narrow = narrowBand(1000, edges(band), narrow, 16)));
  assert.deepEqual(crossing, [false, false, true, true, true, false, false],
    'a panel dragged across the edge and back swaps the lines once each way, at 192 and 208 pixels');
  assert.equal(narrowBand(1000, edges(200), false, 20), true, 'the band is counted in the root font size');
  assert.equal(narrowBand(0, { left: 0, right: 0 }, true, 16), false, 'a hidden office, as in focus mode, has no band to fit into');
});

test('COVER_IN_TICK_ONLY mutant: with no renderer and no frame loop, the office still keeps its covered edges and band beside the panels', () => {
  const resized = [];
  const mutated = [];
  const frames = [];
  const fakes = {
    ResizeObserver: class { constructor(run) { resized.push(run); } observe() {} disconnect() {} },
    MutationObserver: class { constructor(run) { mutated.push(run); } observe() {} disconnect() {} },
    requestAnimationFrame: (run) => frames.push(run),
    cancelAnimationFrame() {},
  };
  const saved = Object.fromEntries(Object.keys(fakes).map((name) => [name, globalThis[name]]));
  Object.assign(globalThis, fakes);
  try {
    const style = {};
    const box = { getBoundingClientRect: () => ({ left: 0, top: 0, right: 900, bottom: 800, width: 900, height: 800 }),
      style: { setProperty: (name, value) => { style[name] = value; } } };
    const panel = (left, right) => ({ getBoundingClientRect: () => ({ left, right, top: 0, bottom: 800 }) });
    let open = [];
    const covers = [];
    watchCover(box, { querySelector: () => null, querySelectorAll: () => open }, 16, (cover) => covers.push(cover));
    resized[0]([]);
    assert.deepEqual(style, { '--covered-left': '0px', '--covered-right': '0px' }, 'the office is measured as soon as it is laid out');
    open = [panel(0, 400), panel(420, 900)];
    mutated[0]([]);
    mutated[0]([]);
    assert.equal(frames.length, 1, 'a burst of panel changes is measured once, in the next frame');
    frames[0]();
    assert.deepEqual(style, { '--covered-left': '400px', '--covered-right': '480px' }, 'Crew and a Control view cover the edges they reach');
    assert.deepEqual(covers.map(({ camera, narrow }) => ({ camera, narrow })), [
      { camera: { left: 0, right: 0 }, narrow: false },
      { camera: { left: 260, right: 340 }, narrow: true },
    ], 'the camera keeps a third of the office, and the twenty pixels between the panels send the lines to the top bar');
  } finally {
    Object.assign(globalThis, saved);
  }
});

test('FRUSTUM_SHIFT mutant: the camera fits the uncovered part and centres the office there', () => {
  const size = { width: 1440, height: 935 };
  const open = frustum(20, 14, size, { left: 240, right: 425 }, 1);
  const plain = frustum(20, 14, size, { left: 0, right: 0 }, 1);
  const centre = (f) => ((0 - f.left) / (f.right - f.left)) * size.width;
  assert.equal(Math.round(centre(plain)), 720);
  assert.equal(Math.round(centre(open)), 628, 'the floor centre sits midway between the panels, (240 + 1015) / 2');
  assert.ok(open.top > plain.top, 'a narrower uncovered part fits the office smaller');
  assert.equal(Number(((open.right - open.left) / (open.top - open.bottom)).toFixed(4)), Number((1440 / 935).toFixed(4)), 'the frustum keeps the canvas aspect');
});

test('OFF_CANVAS_OVERLAY mutant: a button or tag is laid over the canvas only where its point lands on it', () => {
  const size = { width: 800, height: 500 };
  for (const [point, expected] of [
    [{ x: 0, y: 0 }, true], [{ x: 800, y: 500 }, true], [{ x: 400, y: 250 }, true],
    [{ x: -1, y: 250 }, false], [{ x: 801, y: 250 }, false], [{ x: 400, y: -0.5 }, false], [{ x: 400, y: 520 }, false],
  ]) assert.equal(onCanvas(point, size), expected, JSON.stringify(point));
});

test('FULL_LINE_CENTRED and FULL_LINE_CATCHES_CLICKS mutants: the full line sits in the part no panel covers and lets clicks through', async () => {
  const css = await readFile(resolve(import.meta.dirname, '../src/style.css'), 'utf8');
  const rule = /^\.office-full\s*\{([^}]*)\}/m.exec(css)?.[1] ?? '';
  const declared = (property) => new RegExp(`(?:^|;|\\s)${property}:\\s*([^;]+);`).exec(rule)?.[1].trim();
  assert.deepEqual(['left', 'max-width', 'pointer-events', 'transform'].map(declared), [
    'calc(var(--covered-left, 0px) + 0.75rem)',
    'calc(100% - var(--covered-left, 0px) - var(--covered-right, 0px) - 1.5rem)',
    'none',
    undefined,
  ]);
});

test('CHIP_OFF_BOARD mutant: the board card count sits in the middle of the button laid over the board, so on its face', async () => {
  const css = await readFile(resolve(import.meta.dirname, '../src/style.css'), 'utf8');
  const chip = /\.board-note-count\s*\{([^}]*)\}/.exec(css)?.[1] ?? '';
  const declared = (property) => new RegExp(`(?:^|;|\\s)${property}:\\s*([^;]+);`).exec(chip)?.[1].trim();
  assert.deepEqual(['position', 'left', 'top', 'transform'].map(declared), ['absolute', '50%', '50%', 'translate(-50%, -50%)']);
  await withOffice(async (_, server) => {
    const { BoardNotes } = await server.ssrLoadModule('/src/renderer/BoardNotes.tsx');
    assert.match(renderToStaticMarkup(createElement(BoardNotes, { count: 3, hidden: 1, onOpenTasks() {} })),
      /^<button type="button" class="board-select"[^>]*><span class="board-note-count">3 cards · \+1<\/span><\/button>$/);
  });
});

test('RING_ONE_TONE mutant: the focus ring over the office has two tones far enough apart to show on any floor', async () => {
  const css = await readFile(resolve(import.meta.dirname, '../src/style.css'), 'utf8');
  const rule = /\.agent-select:focus-visible,\s*\.board-select:focus-visible\s*\{([^}]*)\}/.exec(css)?.[1];
  assert.ok(rule, 'the office focus ring has its own rule');
  const office = /\.office\s*\{([^}]*)\}/.exec(css)?.[1] ?? '';
  assert.match(office, /overflow:\s*hidden/, 'the office clips what is laid over it');
  assert.match(office, /isolation:\s*isolate/, 'nothing laid over the office rises above the panels floating over it');
  for (const theme of ['light', 'dark']) {
    const tokens = themeTokens(css, theme);
    const resolve = (value) => value.startsWith('var(--') ? tokens[value.slice(6, -1)] : value;
    const outline = resolve(/outline-color:\s*([^;]+);/.exec(rule)?.[1].trim() ?? '');
    const halo = resolve(/box-shadow:\s*0 0 0 \d+px\s*([^;]+);/.exec(rule)?.[1].trim() ?? '');
    assert.match(`${outline}${halo}`, /^#[0-9a-f]{6}#[0-9a-f]{6}$/i, `${theme}: both tones resolve`);
    // Contrast multiplies across a background, so tones 9:1 apart leave one of them at least 3:1 against any floor.
    assert.ok(contrast(outline, halo) >= 9, `${theme}: ${outline} on ${halo} is ${contrast(outline, halo).toFixed(2)}:1`);
    for (const floor of ['#cfb894', '#d8c3a1', '#efe8dc', '#1d2433', '#5a5048']) {
      assert.ok(Math.max(contrast(outline, floor), contrast(halo, floor)) >= 3, `${theme} ring on ${floor}`);
    }
  }
});

test('BADGE_FALLBACK mutant: without a 2D offscreen canvas a badge is a disc in the state colour with a white rim', () => {
  const size = 32;
  const pixels = discPixels(BADGE_COLOURS.working, size);
  const at = (x, y) => [...pixels.slice((y * size + x) * 4, (y * size + x) * 4 + 4)];
  assert.deepEqual(at(16, 16), [0x4b, 0x8f, 0xd6, 255]);
  assert.deepEqual(at(16, 1), [255, 255, 255, 255]);
  assert.deepEqual(at(0, 0), [0, 0, 0, 0]);
  assert.deepEqual([...discPixels(BADGE_COLOURS.done, size).slice((16 * size + 16) * 4, (16 * size + 16) * 4 + 3)], [0x3e, 0x9a, 0x5c]);
});

test('each badge draws its own glyph in its state colour', () => {
  const strokes = (badge) => {
    const calls = [];
    const surface = new Proxy({}, {
      get: (_, name) => (name in surface ? undefined : (...args) => calls.push([name, ...args])),
      set: (_, name, value) => { calls.push([`=${String(name)}`, value]); return true; },
    });
    drawBadge(surface, badge);
    return calls;
  };
  const glyph = (badge) => strokes(badge).filter(([name]) => ['fillRect', 'strokeRect', 'fillText', 'lineTo', 'arc'].includes(name))
    .map((call) => call.join(' '));
  assert.deepEqual(glyph('held'), glyph('paused'), 'held and paused share the pause bars');
  assert.notDeepEqual(glyph('halted'), glyph('paused'));
  assert.ok(glyph('question').includes('fillText ? 48 48'));
  assert.ok(glyph('attached').includes('fillText >_ 48 47'));
  for (const badge of Object.keys(BADGE_COLOURS)) {
    assert.ok(strokes(badge).some(([name, value]) => name === '=fillStyle' && value === BADGE_COLOURS[badge]), badge);
  }
  assert.equal(BADGE_COLOURS.question, BADGE_COLOURS.held, 'waiting and held share the amber of the approved table');
  assert.equal(BADGE_COLOURS.exhausted, '#8d8f93');
});
