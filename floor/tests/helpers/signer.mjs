// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The page's signer on Node's own WebCrypto, with a key store and a lock the test can share
// between two "tabs".

import { webcrypto } from 'node:crypto';

import { createSigner } from '../../src/signer.ts';

/** The origin every fake page in the Floor tests is loaded from. */
export const ORIGIN = 'http://127.0.0.1:7717';
export const NOW = 1_761_100_000_000;

export function memoryKeyStore(initial = null) {
  let held = initial;
  return {
    get held() { return held; },
    async read() { return held; },
    async write(record) { held = record; },
    async clear() { held = null; },
  };
}

export function memoryLock() {
  let tail = Promise.resolve();
  return (run) => {
    const next = tail.then(run, run);
    tail = next.catch(() => {});
    return next;
  };
}

export function testSigner({
  store = memoryKeyStore(), lock = memoryLock(), now = () => NOW, origin = ORIGIN, serviceWorkers = null,
} = {}) {
  const signer = createSigner({
    store,
    lock,
    subtle: webcrypto.subtle,
    random: (bytes) => { webcrypto.getRandomValues(bytes); },
    now,
    origin: () => origin,
    serviceWorkers,
  });
  return { signer, store };
}

/** A signer that already holds a key, as a browser does after it has signed in. */
export async function enrolledSigner(options) {
  const made = testSigner(options);
  const key = await made.signer.newKey();
  await made.signer.keep(key);
  return { ...made, key };
}

export function b64u(bytes) {
  return Buffer.from(bytes).toString('base64url');
}
