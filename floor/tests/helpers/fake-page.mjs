// SPDX-License-Identifier: MIT OR Apache-2.0

/**
 * Just enough of a page for `react-dom/client` to mount a component and run its
 * effects, set as the global `document` and `window` until `restore()`.
 * `layout` stands in for the stylesheet: `rect(element)` answers a box,
 * `clientWidth` and `offsetWidth` widths, and `shown(element)` whether it is
 * drawn, which `focus()` needs.
 */
export function fakePage(layout = {}) {
  const page = { saved: { document: globalThis.document, window: globalThis.window } };
  class Node {
    ownerDocument = page.document;
    childNodes = [];
    parentNode = null;
    get firstChild() { return this.childNodes[0] ?? null; }
    get nextSibling() {
      const siblings = this.parentNode?.childNodes ?? [];
      return siblings[siblings.indexOf(this) + 1] ?? null;
    }
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
    contains(node) { return node === this || this.childNodes.some((child) => child.contains(node)); }
    addEventListener() {}
    removeEventListener() {}
  }
  class Text extends Node {
    nodeType = 3;
    nodeName = '#text';
    constructor(text) { super(); this.nodeValue = text; }
  }
  class Element extends Node {
    nodeType = 1;
    attributes = new Map();
    style = { setProperty(name, value) { this[name] = value; } };
    scrollLeft = 0;
    constructor(tag) { super(); this.nodeName = this.tagName = tag.toUpperCase(); }
    set textContent(text) { this.childNodes = text === '' ? [] : [new Text(text)]; }
    get id() { return this.getAttribute('id') ?? ''; }
    setAttribute(name, value) { this.attributes.set(name, String(value)); }
    removeAttribute(name) { this.attributes.delete(name); }
    getAttribute(name) { return this.attributes.get(name) ?? null; }
    get dataset() {
      return Object.fromEntries([...this.attributes].filter(([name]) => name.startsWith('data-'))
        .map(([name, value]) => [name.slice(5).replace(/-([a-z])/g, (_, letter) => letter.toUpperCase()), value]));
    }
    get clientWidth() { return layout.clientWidth?.(this) ?? 0; }
    get offsetWidth() { return layout.offsetWidth?.(this) ?? 0; }
    getBoundingClientRect() {
      return layout.rect?.(this) ?? { left: 0, right: 0, width: 0, top: 0, bottom: 0, height: 0 };
    }
    getContext() { return null; }
    closest() { return null; }
    matches() { return false; }
    focus() { if (page.document.body.contains(this) && (layout.shown?.(this) ?? true)) page.document.activeElement = this; }
    querySelector(selector) { return this.querySelectorAll(selector)[0] ?? null; }
    querySelectorAll(selector) {
      const [, name, value] = /^\[([a-z-]+)="([^"]*)"\]$/.exec(selector) ?? [];
      const found = [];
      for (const child of this.childNodes) {
        if (!(child instanceof Element)) continue;
        if (child.getAttribute(name) === value) found.push(child);
        found.push(...child.querySelectorAll(selector));
      }
      return found;
    }
  }
  page.document = { nodeType: 9, addEventListener() {}, removeEventListener() {},
    createElement: (tag) => new Element(tag), createTextNode: (text) => new Text(text),
    getElementById: (id) => page.document.body.querySelector(`[id="${id}"]`) };
  page.document.defaultView = { document: page.document, HTMLIFrameElement: class {} };
  page.document.body = new Element('body');
  page.document.activeElement = page.document.body;
  page.host = page.document.body.appendChild(new Element('div'));
  page.restore = () => {
    globalThis.document = page.saved.document;
    globalThis.window = page.saved.window;
    delete globalThis.IS_REACT_ACT_ENVIRONMENT;
  };
  globalThis.document = page.document;
  globalThis.window = page.document.defaultView;
  globalThis.IS_REACT_ACT_ENVIRONMENT = true;
  return page;
}

/** Calls a mounted element's React `onClick`, as a press of Enter on a focused button does. */
export function press(element) {
  const props = element[Object.keys(element).find((key) => key.startsWith('__reactProps$'))];
  props.onClick({ currentTarget: element, target: element, preventDefault() {}, stopPropagation() {} });
}
