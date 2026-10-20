// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A fake daemon's answer to the page's proof request, made the way the real daemon makes it.

import { createHash, createHmac, randomBytes } from 'node:crypto';

export const PROVE_PATH = '/floor/session/prove';
const LABEL = 'dudes-floor-daemon-proof-v1\n';

const reply = (proof) => ({ ok: true, status: 200, json: async () => ({ proof }) });

/** The answer of a daemon that holds the digest of `secret`. */
export function proofFrom(secret, init) {
  const { nonce } = JSON.parse(init.body);
  const key = createHash('sha256').update(secret).digest();
  return reply(createHmac('sha256', key).update(LABEL + nonce).digest('base64url'));
}

/** The answer of a process that does not hold any digest. */
export function proofFromNobody() {
  return reply(randomBytes(32).toString('base64url'));
}
