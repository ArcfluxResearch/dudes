// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The page proves the daemon before it trusts it.
//
// The page keeps a secret and sends it on every request and socket. While the daemon is down, any
// local process can bind its port, and the page would hand that process the secret and take its
// word that the session is gone. So before the secret leaves the page after a load or a reconnect,
// the page sends a fresh random nonce and asks the daemon for a keyed hash of it. The key is the
// SHA-256 digest of the secret, which the daemon stores and a process that was never signed in
// does not hold, so only the real daemon can answer. The answer also names the origin it was made
// for, so a process on the old port cannot pass the page's nonce to a daemon on another port and
// hand the page that daemon's answer. The check uses WebCrypto's `verify`, which
// compares in constant time.

const LABEL = 'dudes-floor-daemon-proof-v1\n';
/**
 * How long one proof stands. The next request may use another connection than the proof did, so a
 * proof is only good for as long as it takes to use it; after this the daemon proves itself again.
 */
export const PROOF_LIFETIME_MS = 2_000;
const PROOF = /^[A-Za-z0-9_-]{43}$/;

/**
 * Where the page stands with the daemon it is talking to.
 *
 * - `unproven`: nothing has been asked since the page loaded or the connection last closed.
 * - `proving`: a nonce is out.
 * - `proven`: the answer verified for the current secret. The secret may be sent.
 * - `refused`: the answer was missing or did not verify. The page keeps its secret and sends it
 *   nowhere, and never signs out because of an answer it could not verify.
 */
export type ProofState = 'unproven' | 'proving' | 'proven' | 'refused';

export interface ProofReply {
  readonly ok: boolean;
  json(): Promise<unknown>;
}

/** What a proof needs from the browser: one POST without the secret, WebCrypto, random bytes, and its own origin. */
export interface ProofPorts {
  post(body: string): Promise<ProofReply>;
  /** The origin the page was loaded from, which the daemon binds into its answer. */
  origin(): string;
  readonly subtle: SubtleCrypto;
  random(bytes: Uint8Array<ArrayBuffer>): void;
}

const encoder = new TextEncoder();

function base64url(bytes: Uint8Array): string {
  let text = '';
  for (const byte of bytes) text += String.fromCharCode(byte);
  return btoa(text).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function fromBase64url(text: string): Uint8Array<ArrayBuffer> {
  const raw = atob(text.replace(/-/g, '+').replace(/_/g, '/'));
  const bytes = new Uint8Array(raw.length);
  for (let index = 0; index < raw.length; index += 1) bytes[index] = raw.charCodeAt(index);
  return bytes;
}

/** One round trip: whether the daemon answered a fresh nonce with the hash only the secret's digest makes. */
export async function checkDaemon(secret: string, ports: ProofPorts): Promise<boolean> {
  const nonceBytes = new Uint8Array(32);
  ports.random(nonceBytes);
  const nonce = base64url(nonceBytes);
  const digest = await ports.subtle.digest('SHA-256', encoder.encode(secret));
  const key = await ports.subtle.importKey('raw', digest, { name: 'HMAC', hash: 'SHA-256' }, false, ['verify']);
  const reply = await ports.post(JSON.stringify({ nonce }));
  if (!reply.ok) return false;
  const answer: unknown = await reply.json();
  if (typeof answer !== 'object' || answer === null || !('proof' in answer)) return false;
  const proof = answer.proof;
  if (typeof proof !== 'string' || !PROOF.test(proof)) return false;
  return ports.subtle.verify('HMAC', key, fromBase64url(proof), encoder.encode(`${LABEL}${ports.origin()}\n${nonce}`));
}

let state: ProofState = 'unproven';
let provenFor: string | null = null;
let provenAt = 0;
let generation = 0;
let inflight: { readonly secret: string; readonly promise: Promise<boolean> } | null = null;
const listeners = new Set<() => void>();

export function getProofState(): ProofState {
  return state;
}

export function subscribeToProof(listener: () => void): () => void {
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
  };
}

function move(next: ProofState): void {
  if (state === next) return;
  state = next;
  for (const listener of [...listeners]) listener();
}

/** Whether the daemon has proved itself for this secret on the connection the page holds now. */
export function isProven(secret: string): boolean {
  return state === 'proven' && provenFor === secret && Date.now() - provenAt < PROOF_LIFETIME_MS;
}

/**
 * The connection to the daemon ended or failed, so whatever answers next has to prove itself again.
 * A refusal stays on screen until a later proof settles it.
 */
export function forgetProof(): void {
  generation += 1;
  provenFor = null;
  inflight = null;
  if (state !== 'refused') move('unproven');
}

/** What a reload does. For tests; the page gets a fresh module per load. */
export function resetProof(): void {
  forgetProof();
  state = 'unproven';
  listeners.clear();
}

/**
 * Prove the daemon for this secret, or say it could not. Callers that find it already proven pay
 * nothing; callers that arrive while a proof is out share it.
 */
export function proveDaemon(secret: string, ports: ProofPorts): Promise<boolean> {
  if (isProven(secret)) return Promise.resolve(true);
  if (inflight?.secret === secret) return inflight.promise;
  const started = generation;
  move('proving');
  const promise = (async (): Promise<boolean> => {
    let verified = false;
    try {
      verified = await checkDaemon(secret, ports);
    } catch {
      verified = false;
    }
    if (started !== generation) {
      // The connection moved on while the nonce was out: this answer belongs to the old one.
      return false;
    }
    inflight = null;
    provenFor = verified ? secret : null;
    provenAt = Date.now();
    move(verified ? 'proven' : 'refused');
    return verified;
  })();
  inflight = { secret, promise };
  return promise;
}
