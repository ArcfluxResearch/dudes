// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The pure side of sending files in Chat: what a chip is, the block of paths a message carries, and
// how the daemon's answer to an upload reads.

import { trimAsciiSpace, utf8Bytes } from '../onboarding/hire.ts';
import { exactObject } from './model.ts';

/** The most one file may be, checked here before anything is sent and again by the daemon. */
export const MAX_FILE_BYTES = 10 * 1024 * 1024;
export const MAX_NOTE_BYTES = 4096;

/** One file the person attached, from the moment it is picked to the moment the message is queued. */
export interface ChatFile {
  readonly id: number;
  readonly name: string;
  readonly size: number;
  readonly status: 'uploading' | 'ready' | 'failed';
  /** The copy's absolute path on the daemon's machine, once uploaded. */
  readonly path: string | null;
  /** The name the daemon kept the copy under. */
  readonly kept: string | null;
  readonly error: string | null;
}

export type UploadResult =
  | { readonly kind: 'kept'; readonly path: string; readonly name: string }
  | { readonly kind: 'busy' }
  | { readonly kind: 'failed'; readonly message: string };

export function formatBytes(bytes: number): string {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
  if (bytes >= 1024) return `${Math.round(bytes / 1024)} KiB`;
  return `${bytes} B`;
}

export function tooBig(name: string, size: number): string {
  return `${name} is ${formatBytes(size)}. Files can be up to 10 MiB.`;
}

/** The block that follows the person's text: a heading and one line per file. */
export function attachedBlock(files: readonly ChatFile[]): string {
  if (files.length === 0) return '';
  return `Attached files:\n${files.map(file => `- ${file.path} (${file.kept})`).join('\n')}`;
}

/** The text one message sends: the typed text, a blank line, then the block. */
export function messageText(draft: string, files: readonly ChatFile[]): string {
  const text = trimAsciiSpace(draft);
  const block = attachedBlock(files);
  if (block === '') return text;
  return text === '' ? block : `${text}\n\n${block}`;
}

/** Why a message with these files cannot be sent yet, or `null`. */
export function filesProblem(draft: string, files: readonly ChatFile[]): string | null {
  if (files.length === 0) return null;
  const bytes = utf8Bytes(messageText(draft, files));
  return bytes > MAX_NOTE_BYTES
    ? `The message and the file paths are ${bytes} UTF-8 bytes; the limit is ${MAX_NOTE_BYTES}. Remove a file or shorten the message.`
    : null;
}

const REFUSED: Readonly<Record<string, string>> = {
  agent_absent: 'The agent is no longer on the floor.',
  agent_retired: 'The agent was retired.',
  attached: 'Files work with agents Dudes launches.',
  not_a_folder: 'This agent\'s files folder is not a real folder, so nothing was saved.',
};

export function decodeUpload(status: number, text: string): UploadResult {
  let body: unknown;
  try { body = JSON.parse(text) as unknown; } catch { body = null; }
  if (status === 200 && exactObject(body, ['busy']) && body.busy === true) return { kind: 'busy' };
  if (status === 200 && exactObject(body, ['path', 'name', 'bytes']) && typeof body.path === 'string'
    && typeof body.name === 'string' && typeof body.bytes === 'number') {
    return { kind: 'kept', path: body.path, name: body.name };
  }
  if (status === 409 && exactObject(body, ['state', 'reason', 'folder']) && body.state === 'refused'
    && body.reason === 'folder_full' && typeof body.folder === 'string') {
    return { kind: 'failed', message: `This agent's files folder is full (200 MiB). Delete files in ${body.folder} to make room.` };
  }
  if (status === 409 && exactObject(body, ['state', 'reason']) && body.state === 'refused'
    && typeof body.reason === 'string' && body.reason in REFUSED) {
    return { kind: 'failed', message: REFUSED[body.reason] as string };
  }
  if (status === 413) return { kind: 'failed', message: 'This file is over the 10 MiB limit.' };
  if (status === 401 || status === 403) return { kind: 'failed', message: 'This page is not signed in, so nothing was uploaded.' };
  return { kind: 'failed', message: 'The upload failed. Try again.' };
}
