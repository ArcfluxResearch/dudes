// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A fake daemon's answer to the page's proof request, made the way the real daemon makes it.

import { createHash, createHmac, randomBytes } from 'node:crypto';

export const PROVE_PATH = '/floor/session/prove';
/** The origin every fake page in the Floor tests is loaded from. */
export const ORIGIN = 'http://127.0.0.1:7717';
const LABEL = 'dudes-floor-daemon-proof-v1\n';

const reply = (proof) => ({ ok: true, status: 200, json: async () => ({ proof }) });

/** The answer of a daemon that holds the digest of `secret`. */
export function proofFrom(secret, init, origin = ORIGIN) {
  const { nonce } = JSON.parse(init.body);
  const key = createHash('sha256').update(secret).digest();
  return reply(createHmac('sha256', key).update(`${LABEL}${origin}\n${nonce}`).digest('base64url'));
}

/** The answer of a process that does not hold any digest. */
export function proofFromNobody() {
  return reply(randomBytes(32).toString('base64url'));
}
