// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The one place the page's sign-in key lives.
//
// The browser makes a P-256 key that it will not export and keeps it in IndexedDB, where every tab
// of the browser shares it. Every request and socket handshake the page makes is signed with it, so
// nothing the page holds can be copied out and replayed elsewhere. Nothing here falls back to a
// secret the page can read: a browser that cannot hold the key is not signed in.

const LABEL = 'dudes-floor-request-v1';
const LOCK_NAME = 'dudes-floor-sign-in';
const DATABASE = 'dudes-floor';
const STORE = 'keys';
const RECORD = 'device';
const KEY_ALGORITHM = { name: 'ECDSA', namedCurve: 'P-256' } as const;
const SIGNATURE_ALGORITHM = { name: 'ECDSA', hash: 'SHA-256' } as const;

/** This browser cannot hold a sign-in key, so the page signs nothing. */
export class KeyUnavailable extends Error {
  constructor() {
    super('This browser cannot keep a sign-in key');
  }
}

/**
 * A service worker controls this page. Unregistering it does not release a page it already
 * controls, so the page signs nothing until it is reloaded.
 */
export class ReloadNeeded extends Error {
  constructor() {
    super('Reload this page to finish signing in.');
  }
}

/** A request body the page cannot sign exactly as it sends it. */
export class UnsignableBody extends TypeError {
  constructor() {
    super('Floor requests send text or bytes only');
  }
}

export interface KeyRecord {
  readonly device: string;
  readonly privateKey: CryptoKey;
}

/** A key the page just made, not yet kept. `spki` is what the daemon is told. */
export interface NewKey extends KeyRecord {
  readonly spki: string;
}

/** Where the key is kept between loads, shared by every tab of the browser. */
export interface KeyStore {
  read(): Promise<KeyRecord | null>;
  write(record: KeyRecord): Promise<void>;
  /** Delete the record, or only the one for `device` when given, compared and deleted in one transaction. */
  clear(device?: string): Promise<void>;
}

export interface SignerPorts {
  readonly store: KeyStore;
  /** Runs one thing at a time across every tab of the browser. */
  lock<T>(run: () => Promise<T>): Promise<T>;
  readonly subtle: SubtleCrypto;
  random(bytes: Uint8Array<ArrayBuffer>): void;
  now(): number;
  origin(): string;
  readonly serviceWorkers: {
    readonly controller: unknown;
    getRegistrations(): Promise<readonly { unregister(): Promise<boolean> }[]>;
  } | null;
}

/** The headers a signed request carries and the entries a signed socket offers. */
export interface Signature {
  readonly origin: string;
  readonly time: string;
  readonly request: string;
  readonly signature: string;
}

export interface Signer {
  /** Unregisters service workers; rejects with `ReloadNeeded` when one controls this page. */
  ready(): Promise<void>;
  /** The stored key's device id, or `null` when this browser holds none. */
  device(): Promise<string | null>;
  newKey(): Promise<NewKey>;
  keep(key: KeyRecord): Promise<void>;
  /** Delete the key, but only the one for `device` when given, so another tab's newer key stays. */
  forget(device?: string): Promise<void>;
  lock<T>(run: () => Promise<T>): Promise<T>;
  /** Sign a request made now with a key the page made but has not kept, as the link exchange does. */
  signWith(key: KeyRecord, method: string, url: string, body: Uint8Array<ArrayBuffer>): Promise<Signature>;
  /** Sign a request made now. `null` when the browser holds no key. */
  sign(method: string, url: string, body: Uint8Array<ArrayBuffer>): Promise<Signature | null>;
  /** The `Sec-WebSocket-Protocol` entries after the page's own protocol, or `null` when the browser holds no key. */
  socketEntries(url: string): Promise<readonly string[] | null>;
}

const encoder = new TextEncoder();

/** The headers a signed request carries. */
export function signatureHeaders(found: Signature): Readonly<Record<string, string>> {
  return {
    'X-Dudes-Origin': found.origin,
    'X-Dudes-Time': found.time,
    'X-Dudes-Request': found.request,
    'X-Dudes-Signature': found.signature,
  };
}

export function base64url(bytes: Uint8Array<ArrayBuffer>): string {
  let text = '';
  for (const byte of bytes) text += String.fromCharCode(byte);
  return btoa(text).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function field(value: Uint8Array<ArrayBuffer>): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(4 + value.length);
  new DataView(out.buffer).setUint32(0, value.length);
  out.set(value, 4);
  return out;
}

export interface MessageParts {
  readonly device: string;
  readonly verb: string;
  readonly target: string;
  readonly origin: string;
  readonly bodyHash: Uint8Array<ArrayBuffer>;
  readonly time: string;
  readonly request: string;
}

/** The bytes the key signs: eight length-prefixed fields, in the order the daemon reads them. */
export function signedMessage(parts: MessageParts): Uint8Array<ArrayBuffer> {
  const fields = [
    encoder.encode(LABEL),
    encoder.encode(parts.device),
    encoder.encode(parts.verb),
    encoder.encode(parts.target),
    encoder.encode(parts.origin),
    parts.bodyHash,
    encoder.encode(parts.time),
    encoder.encode(parts.request),
  ].map(field);
  const out = new Uint8Array(fields.reduce((sum, part) => sum + part.length, 0));
  let at = 0;
  for (const part of fields) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

/** The request line's target as the daemon sees it: the path and the query. */
export function requestTarget(url: string, origin: string): string {
  const parsed = new URL(url, origin);
  return parsed.pathname + parsed.search;
}

export function createSigner(ports: SignerPorts): Signer {
  const { subtle } = ports;

  let prepared: Promise<boolean> | null = null;
  function prepare(): Promise<boolean> {
    prepared ??= (async () => {
      try {
        const registrations = await ports.serviceWorkers?.getRegistrations();
        await Promise.all((registrations ?? []).map((registration) => registration.unregister()));
      } catch {}
      return Boolean(ports.serviceWorkers?.controller);
    })();
    return prepared;
  }

  async function ready(): Promise<void> {
    if (await prepare()) throw new ReloadNeeded();
  }

  async function guard<T>(run: () => Promise<T>): Promise<T> {
    await ready();
    try {
      return await run();
    } catch {
      throw new KeyUnavailable();
    }
  }

  async function signWith(record: KeyRecord, method: string, url: string, body: Uint8Array<ArrayBuffer>): Promise<Signature> {
    const idBytes = new Uint8Array(16);
    ports.random(idBytes);
    const time = String(ports.now());
    const request = base64url(idBytes);
    const bodyHash = new Uint8Array(await subtle.digest('SHA-256', body));
    const origin = ports.origin();
    const message = signedMessage({
      device: record.device,
      verb: method,
      target: requestTarget(url, origin),
      origin,
      bodyHash,
      time,
      request,
    });
    const signature = new Uint8Array(await subtle.sign(SIGNATURE_ALGORITHM, record.privateKey, message));
    return { origin, time, request, signature: base64url(signature) };
  }

  async function signed(method: string, url: string, body: Uint8Array<ArrayBuffer>): Promise<Signature | null> {
    const record = await guard(() => ports.store.read());
    return record === null ? null : guard(() => signWith(record, method, url, body));
  }

  return {
    ready,
    device: () => guard(async () => (await ports.store.read())?.device ?? null),
    newKey: () => guard(async () => {
      const pair = await subtle.generateKey(KEY_ALGORITHM, false, ['sign', 'verify']);
      const spkiBytes = new Uint8Array(await subtle.exportKey('spki', pair.publicKey));
      const device = base64url(new Uint8Array(await subtle.digest('SHA-256', spkiBytes)));
      return { device, privateKey: pair.privateKey, spki: base64url(spkiBytes) };
    }),
    keep: (key) => guard(() => ports.store.write({ device: key.device, privateKey: key.privateKey })),
    forget: (device) => guard(() => ports.store.clear(device)),
    lock: (run) => ports.lock(run),
    signWith: (key, method, url, body) => guard(() => signWith(key, method, url, body)),
    sign: signed,
    socketEntries: async (url) => {
      const found = await signed('GET', url, new Uint8Array(0));
      return found === null ? null : Object.freeze([`dudes-sig.${found.signature}`, `dudes-time.${found.time}`, `dudes-id.${found.request}`]);
    },
  };
}

/** Resolves when `target` fires `success`, and rejects when it fires `error`. */
function settled(target: EventTarget, success: string): Promise<void> {
  return new Promise((resolve, reject) => {
    target.addEventListener(success, () => resolve(), { once: true });
    target.addEventListener('error', () => reject(new Error('storage failed')), { once: true });
  });
}

async function openDatabase(): Promise<IDBDatabase> {
  const opening = indexedDB.open(DATABASE, 1);
  opening.addEventListener('upgradeneeded', () => {
    opening.result.createObjectStore(STORE);
  });
  await settled(opening, 'success');
  return opening.result;
}

async function inStore<T>(mode: IDBTransactionMode, run: (store: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  const database = await openDatabase();
  try {
    const transaction = database.transaction(STORE, mode);
    const request = run(transaction.objectStore(STORE));
    await settled(transaction, 'complete');
    return request.result;
  } finally {
    database.close();
  }
}

function isRecord(value: unknown): value is KeyRecord {
  if (typeof value !== 'object' || value === null) return false;
  const row = value as Record<string, unknown>;
  return typeof row.device === 'string' && typeof row.privateKey === 'object' && row.privateKey !== null;
}

const indexedDbStore: KeyStore = {
  async read() {
    const found: unknown = await inStore('readonly', (store) => store.get(RECORD));
    return isRecord(found) ? found : null;
  },
  async write(record) {
    await inStore('readwrite', (store) => store.put({ device: record.device, privateKey: record.privateKey }, RECORD));
  },
  async clear(device) {
    const database = await openDatabase();
    try {
      const transaction = database.transaction(STORE, 'readwrite');
      const store = transaction.objectStore(STORE);
      const done = settled(transaction, 'complete');
      const found = store.get(RECORD);
      await settled(found, 'success');
      const held: unknown = found.result;
      if (device === undefined || (isRecord(held) && held.device === device)) store.delete(RECORD);
      await done;
    } finally {
      database.close();
    }
  },
};

/** What a browser must offer to hold the key. */
export interface BrowserCapabilities {
  readonly subtle?: unknown;
  readonly indexedDB?: unknown;
  readonly locks?: unknown;
}

export function canHoldKey(found: BrowserCapabilities): boolean {
  return Boolean(found.subtle) && Boolean(found.indexedDB) && Boolean(found.locks);
}

/** The signer for this browser, or `null` when it lacks WebCrypto, IndexedDB or locks. */
export function createBrowserSigner(): Signer | null {
  const found: BrowserCapabilities = {
    subtle: globalThis.crypto?.subtle,
    indexedDB: globalThis.indexedDB,
    locks: globalThis.navigator?.locks,
  };
  if (!canHoldKey(found)) return null;
  return createSigner({
    store: indexedDbStore,
    lock: (run) => navigator.locks.request(LOCK_NAME, run),
    subtle: crypto.subtle,
    random: (bytes) => {
      crypto.getRandomValues(bytes);
    },
    now: () => Date.now(),
    origin: () => window.location.origin,
    serviceWorkers: navigator.serviceWorker ?? null,
  });
}

let installed: Signer | null | undefined;

/** The page's signer; `null` when this browser cannot hold a key. Made on first use. */
export function floorSigner(): Signer | null {
  if (installed === undefined) installed = createBrowserSigner();
  return installed;
}

/** Replace the page's signer. For tests; the page gets a fresh module per load. */
export function installSigner(signer: Signer | null | undefined): void {
  installed = signer;
}

/** What a socket offers after the page's own protocol: signed now, or `null` when the browser holds no key. */
export function socketOffer(signer: Signer | null): (url: string) => Promise<readonly string[] | null> {
  return async (url) => {
    if (signer === null) return null;
    try {
      return await signer.socketEntries(url);
    } catch {
      return null;
    }
  };
}
