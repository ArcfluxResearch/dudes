// A lazy chunk adds its own preload and stylesheet tags to the document after
// the page opened. They are not part of the build the page was loaded from.
import assert from 'node:assert/strict';
import test from 'node:test';

import { installSigner } from '../src/signer.ts';
import { fakeDaemon } from './helpers/fake-daemon.mjs';
import { testSigner } from './helpers/signer.mjs';

const BOOTSTRAP = `A${'b'.repeat(41)}E`;
const OWN = ['/floor/assets/index-aaaa1111.js', '/floor/assets/index-bbbb2222.css'];

test('a lazy chunk loaded after the page opened does not change which build the page is', async () => {
  const tag = (attribute, ref) => ({ getAttribute: (name) => (name === attribute ? ref : null) });
  const elements = [tag('src', OWN[0]), tag('href', OWN[1])];
  const location = { hash: `#bootstrap=${BOOTSTRAP}`, pathname: '/floor/', search: '' };
  const fetched = [];
  const daemon = fakeDaemon({ links: [BOOTSTRAP] });
  installSigner(testSigner().signer);
  const window = {
    location,
    history: { replaceState() { location.hash = ''; } },
    fetch: async (url, init) => {
      fetched.push(`${init.method ?? 'GET'} ${url}`);
      if (init.method === 'GET') {
        const html = `<script type="module" crossorigin src="${OWN[0]}"></script><link rel="stylesheet" crossorigin href="${OWN[1]}">`;
        return { ok: true, status: 200, async text() { return html; } };
      }
      return daemon.plain(url, init);
    },
  };
  Object.assign(globalThis, { document: { querySelectorAll: () => elements }, window });
  try {
    const session = await import('../src/session.ts?loaded-with');
    elements.push(tag('href', '/floor/assets/grid-dddd4444.css'), tag('href', '/floor/assets/OfficeRenderer-eeee5555.js'));
    assert.equal(typeof (await session.reauthenticateFloorSession()).device, 'string');
    assert.deepEqual(fetched, ['GET /floor/', 'POST /floor/session']);
  } finally {
    installSigner(undefined);
    delete globalThis.document;
    delete globalThis.window;
  }
});
