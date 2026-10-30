// SPDX-License-Identifier: MIT OR Apache-2.0
//
// A daemon's side of the sign-in, written from the wire rules and not from the page's code: it
// spends links once, enrols the key it is sent, and checks every signed request the way the real
// daemon does, with WebCrypto, so a page that signs the wrong bytes is refused here.

import { createHash, webcrypto } from 'node:crypto';

import { NOW, ORIGIN } from './signer.mjs';

const { subtle } = webcrypto;
const encoder = new TextEncoder();

export const reply = (status, payload) => ({
  ok: status >= 200 && status < 300,
  status,
  json: async () => payload,
  clone() { return reply(status, payload); },
  text: async () => JSON.stringify(payload),
});

function field(bytes) {
  const out = Buffer.alloc(4 + bytes.length);
  out.writeUInt32BE(bytes.length);
  Buffer.from(bytes).copy(out, 4);
  return out;
}

/** The eight fields of the signed message, as the wire rules list them. */
export function wireMessage({ device, method, target, origin, body, time, request }) {
  return Buffer.concat([
    'dudes-floor-request-v1', device, method, target, origin,
    createHash('sha256').update(body).digest(), time, request,
  ].map((part) => field(typeof part === 'string' ? encoder.encode(part) : part)));
}

export function fakeDaemon({ links = [], now = () => NOW, origin = ORIGIN } = {}) {
  const live = new Map();
  const seen = new Set();
  const state = {
    exchanges: 0,
    exchangeHeaders: [],
    requests: [],
    refuseNext: 0,
    /** Answers a request before the daemon looks at it: a response, or `undefined` to let it through. */
    override: null,
    unreachable: false,
    endStatus: 200,
    get devices() { return [...live.keys()]; },
    remove(device) { live.delete(device); },
  };
  const unspent = new Set(links);

  async function exchange(init) {
    state.exchanges += 1;
    if (state.unreachable) throw new TypeError('offline');
    const headers = new Headers(init.headers);
    state.exchangeHeaders.push(headers);
    const { link, key } = JSON.parse(init.body);
    const spki = Buffer.from(key, 'base64url');
    const device = createHash('sha256').update(spki).digest('base64url');
    const verifier = await subtle.importKey('spki', spki, { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify']);
    const time = headers.get('x-dudes-time') ?? '';
    const request = headers.get('x-dudes-request') ?? '';
    const message = wireMessage({
      device, method: init.method, target: '/floor/session', origin, body: Buffer.from(init.body), time, request,
    });
    const verified = headers.get('x-dudes-origin') === origin && Math.abs(Number(time) - now()) <= 60_000
      && await subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, verifier, Buffer.from(headers.get('x-dudes-signature') ?? '', 'base64url'), message);
    if (!verified) return reply(401, { signature: 'refused' });
    if (!unspent.delete(link)) return reply(403, {});
    live.set(device, verifier);
    seen.add(request);
    return reply(200, { device });
  }

  async function signed(url, init) {
    if (state.unreachable) throw new TypeError('offline');
    const headers = new Headers(init.headers);
    state.requests.push({ url, init, headers });
    const overridden = state.override?.(url);
    if (overridden !== undefined) return overridden;
    if (state.refuseNext > 0) {
      state.refuseNext -= 1;
      return reply(401, { signature: 'refused' });
    }
    const body = init.body === undefined ? new Uint8Array(0) : init.body;
    const time = headers.get('x-dudes-time') ?? '';
    const request = headers.get('x-dudes-request') ?? '';
    const signature = headers.get('x-dudes-signature') ?? '';
    const claimed = [...live.entries()];
    if (headers.get('x-dudes-origin') !== origin || Math.abs(Number(time) - now()) > 60_000 || seen.has(request)) {
      return reply(401, { signature: 'refused' });
    }
    for (const [device, key] of claimed) {
      const message = wireMessage({ device, method: init.method, target: url, origin, body, time, request });
      if (await subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, key, Buffer.from(signature, 'base64url'), message)) {
        seen.add(request);
        if (url === '/floor/session/end') {
          if (state.endStatus === 200) live.delete(device);
          return reply(state.endStatus, { session: 'ended' });
        }
        return reply(200, { session: 'live' });
      }
    }
    return claimed.length === 0 ? reply(401, { session: 'unknown' }) : reply(401, { signature: 'refused' });
  }

  /** Whether a socket handshake is signed for `url` by a live device: four entries, the protocol first. */
  async function socket(url, protocols) {
    if (protocols.length !== 4 || protocols[0] !== 'dudes-floor-v6') return false;
    const [signature, time, request] = ['dudes-sig.', 'dudes-time.', 'dudes-id.']
      .map((prefix, at) => (protocols[at + 1].startsWith(prefix) ? protocols[at + 1].slice(prefix.length) : null));
    if (signature === null || time === null || request === null) return false;
    if (Math.abs(Number(time) - now()) > 60_000 || seen.has(request)) return false;
    for (const [device, key] of live) {
      const message = wireMessage({ device, method: 'GET', target: url, origin, body: new Uint8Array(0), time, request });
      if (await subtle.verify({ name: 'ECDSA', hash: 'SHA-256' }, key, Buffer.from(signature, 'base64url'), message)) {
        seen.add(request);
        return true;
      }
    }
    return false;
  }

  return {
    state,
    socket,
    /** A device the daemon already knows, as after a sign-in. */
    async enrol(key) {
      const spki = Buffer.from(key.spki, 'base64url');
      live.set(key.device, await subtle.importKey('spki', spki, { name: 'ECDSA', namedCurve: 'P-256' }, false, ['verify']));
    },
    /** The page's request without a signature, for the link exchange. */
    plain: async (url, init) => {
      if (url === '/floor/session') return exchange(init);
      return reply(404, {});
    },
    /** What the browser's network sends a signed request to. */
    send: signed,
  };
}
