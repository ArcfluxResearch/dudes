import { readFile } from 'node:fs/promises';
import { resolve } from 'node:path';

const src = resolve(import.meta.dirname, '../../src');

/** The files under `src/legacy`, in the order the Tailwind entry file imports them. */
export async function legacyFileNames() {
  const entry = await readFile(resolve(src, 'tailwind.css'), 'utf8');
  return [...entry.matchAll(/@import '\.\/legacy\/([\w-]+\.css)';/g)].map(([, name]) => name);
}

/** A legacy stylesheet's rules without the layer statement and the wrapper that put them in the legacy layer. */
export function unwrapLegacy(text) {
  const open = text.indexOf('@layer legacy {');
  const close = text.lastIndexOf('}');
  return text.slice(open + '@layer legacy {'.length, close).trim();
}

/** The older screens' rules as one text, in cascade order, for a test that reads a rule by its selector. */
export async function legacyStyles() {
  const parts = [];
  for (const name of await legacyFileNames()) parts.push(unwrapLegacy(await readFile(resolve(src, 'legacy', name), 'utf8')));
  return parts.join('\n\n');
}
