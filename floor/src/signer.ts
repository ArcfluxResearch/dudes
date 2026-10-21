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

/** A request body the page cannot sign exactly as it sends it. */
export class UnsignableBody extends TypeError {
  constructor() {
    super('Floor requests send text bodies only');
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
  clear(): Promise<void>;
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
  /** The stored key's device id, or `null` when this browser holds none. */
  device(): Promise<string | null>;
  newKey(): Promise<NewKey>;
  keep(key: KeyRecord): Promise<void>;
  /** Delete the key, but only the one for `device` when given, so another tab's newer key stays. */
  forget(device?: string): Promise<void>;
  lock<T>(run: () => Promise<T>): Promise<T>;
  /** Sign a request made now. `null` when the browser holds no key. */
  sign(method: string, url: string, body: Uint8Array<ArrayBuffer>): Promise<Signature | null>;
  /** The `Sec-WebSocket-Protocol` entries after the page's own protocol, or `null` when the browser holds no key. */
  socketEntries(url: string): Promise<readonly string[] | null>;
}

const encoder = new TextEncoder();

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
  readonly method: string;
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
    encoder.encode(parts.method),
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

  let prepared: Promise<void> | null = null;
  function prepare(): Promise<void> {
    prepared ??= (async () => {
      try {
        const registrations = await ports.serviceWorkers?.getRegistrations();
        await Promise.all((registrations ?? []).map((registration) => registration.unregister()));
      } catch {}
    })();
    return prepared;
  }

  async function guard<T>(run: () => Promise<T>): Promise<T> {
    await prepare();
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
    const message = signedMessage({
      device: record.device,
      method,
      target: requestTarget(url, ports.origin()),
      origin: ports.origin(),
      bodyHash,
      time,
      request,
    });
    const signature = new Uint8Array(await subtle.sign(SIGNATURE_ALGORITHM, record.privateKey, message));
    return { origin: ports.origin(), time, request, signature: base64url(signature) };
  }

  return {
    device: () => guard(async () => (await ports.store.read())?.device ?? null),
    newKey: () => guard(async () => {
      const pair = await subtle.generateKey(KEY_ALGORITHM, false, ['sign', 'verify']);
      const spkiBytes = new Uint8Array(await subtle.exportKey('spki', pair.publicKey));
      const device = base64url(new Uint8Array(await subtle.digest('SHA-256', spkiBytes)));
      return { device, privateKey: pair.privateKey, spki: base64url(spkiBytes) };
    }),
    keep: (key) => guard(() => ports.store.write({ device: key.device, privateKey: key.privateKey })),
    forget: (device) => guard(async () => {
      if (device !== undefined && (await ports.store.read())?.device !== device) return;
      await ports.store.clear();
    }),
    lock: (run) => ports.lock(run),
    sign: async (method, url, body) => {
      const record = await guard(() => ports.store.read());
      if (record === null) return null;
      return guard(() => signWith(record, method, url, body));
    },
    socketEntries: async (url) => {
      const record = await guard(() => ports.store.read());
      if (record === null) return null;
      const signed = await guard(() => signWith(record, 'GET', url, new Uint8Array(0)));
      return Object.freeze([`dudes-sig.${signed.signature}`, `dudes-time.${signed.time}`, `dudes-id.${signed.request}`]);
    },
  };
}

function request<T>(request: IDBRequest<T>): Promise<T> {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

function openDatabase(): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    const opening = indexedDB.open(DATABASE, 1);
    opening.onupgradeneeded = () => {
      opening.result.createObjectStore(STORE);
    };
    opening.onsuccess = () => resolve(opening.result);
    opening.onerror = () => reject(opening.error);
    opening.onblocked = () => reject(new Error('blocked'));
  });
}

async function inStore<T>(mode: IDBTransactionMode, run: (store: IDBObjectStore) => IDBRequest<T>): Promise<T> {
  const database = await openDatabase();
  try {
    const transaction = database.transaction(STORE, mode);
    const done = new Promise<void>((resolve, reject) => {
      transaction.oncomplete = () => resolve();
      transaction.onerror = () => reject(transaction.error);
      transaction.onabort = () => reject(transaction.error);
    });
    const result = await request(run(transaction.objectStore(STORE)));
    await done;
    return result;
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
  async clear() {
    await inStore('readwrite', (store) => store.delete(RECORD));
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
