// SPDX-License-Identifier: MIT OR Apache-2.0

const ID = /^[0-7][0-9A-HJKMNP-TV-Z]{25}$/;
const ORDER = /^(0|[1-9][0-9]*)$/;
const MAX_LSN = 18_446_744_073_709_551_615n;
const UTF8 = new TextEncoder();
export const MAX_MESSAGES = 128;
export const MAX_THREADS = 128;
const MAX_THREAD_BYTES = 65_536;
const MAX_MESSAGE_BYTES = 65_536;

export interface MessageRow {
  readonly id: string;
  readonly from: string;
  readonly to: string;
  readonly at: number;
  readonly lsn: string;
  readonly state: 'durable' | 'notified' | 'acked' | 'bounced';
  readonly thread?: string;
  readonly subject: string | null;
  readonly act: 'message' | 'request' | null;
  readonly reply_status: 'not_expected' | 'answered' | 'undeliverable' | 'thread_full' | 'awaiting';
}

export function messageCut(value: unknown): string | null {
  return typeof value === 'string' && value.length <= 20 && ORDER.test(value) && BigInt(value) <= MAX_LSN
    ? value : null;
}

export function decodeMessages(value: unknown, cut: string | null): {
  readonly rows: readonly MessageRow[]; readonly rejected: number;
} {
  const empty = (rejected: number) => ({ rows: Object.freeze([]), rejected });
  if (cut === null) return empty(value === undefined ? 0 : 1);
  if (!Array.isArray(value) || value.length > MAX_MESSAGES
    || UTF8.encode(JSON.stringify(value)).length > MAX_MESSAGE_BYTES) return empty(1);
  const rows: MessageRow[] = [];
  const ids = new Set<string>();
  let previous = 0n;
  for (const raw of value) {
    if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return empty(1);
    const row = raw as Record<string, unknown>;
    const lsn = messageCut(row.lsn);
    if ((Object.keys(row).length !== (row.thread === undefined ? 9 : 10))
      || (row.thread !== undefined && (typeof row.thread !== 'string' || !ID.test(row.thread)))
      || typeof row.id !== 'string' || !ID.test(row.id) || ids.has(row.id)
      || typeof row.from !== 'string' || !ID.test(row.from)
      || typeof row.to !== 'string' || !ID.test(row.to)
      || (row.subject !== null && (typeof row.subject !== 'string'
        || UTF8.encode(row.subject).length > 120 || /[\p{Cc}\p{Cf}\u2028\u2029]/u.test(row.subject)
        || row.subject.trim() !== row.subject || /[^\S ]| {2}/u.test(row.subject)))
      || (row.act !== null && row.act !== 'message' && row.act !== 'request')
      || (row.reply_status !== 'not_expected' && row.reply_status !== 'answered'
        && row.reply_status !== 'undeliverable' && row.reply_status !== 'thread_full' && row.reply_status !== 'awaiting')
      || (row.act === 'request' ? row.reply_status === 'not_expected' : row.reply_status !== 'not_expected')
      || typeof row.at !== 'number'  || !Number.isFinite(row.at)
      || lsn === null || BigInt(lsn) <= previous || BigInt(lsn) > BigInt(cut)
      || (row.state !== 'durable' && row.state !== 'notified' && row.state !== 'acked' && row.state !== 'bounced')) {
      return empty(1);
    }
    rows.push(Object.freeze({ id: row.id, from: row.from, to: row.to, at: row.at, lsn, state: row.state, subject: row.subject, act: row.act, reply_status: row.reply_status,
      ...(typeof row.thread === 'string' ? { thread: row.thread } : {}) }));
    ids.add(row.id);
    previous = BigInt(lsn);
  }
  return { rows: Object.freeze(rows), rejected: 0 };
}

export interface ThreadRow {
  readonly id: string;
  readonly members: readonly string[];
}

export interface MailWindow {
  readonly rows: readonly MessageRow[];
  readonly threads: readonly ThreadRow[];
  readonly cut: string | null;
  readonly rejected: number;
}

export function decodeMailWindow(
  messages: unknown, messageLsn: unknown, threads: unknown, threadLsn: unknown,
): MailWindow {
  const empty = (rejected: number): MailWindow => ({ rows: Object.freeze([]),
    threads: Object.freeze([]), cut: null, rejected });
  if (messages === undefined && messageLsn === undefined && threads === undefined && threadLsn === undefined) return empty(0);
  const cut = messageCut(messageLsn);
  if (cut === null || cut !== messageCut(threadLsn)) return empty(1);
  const decoded = decodeMessages(messages, cut);
  if (decoded.rejected !== 0 || !Array.isArray(threads) || threads.length > MAX_THREADS
    || UTF8.encode(JSON.stringify(threads)).length > MAX_THREAD_BYTES
    || UTF8.encode(JSON.stringify(messages)).length + UTF8.encode(JSON.stringify(threads)).length > MAX_MESSAGE_BYTES) return empty(1);
  const graph = new Map<string, ThreadRow>();
  let previous = '';
  for (const raw of threads) {
    if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) return empty(1);
    const row = raw as Record<string, unknown>;
    const members = row.members;
    if (Object.keys(row).length !== 2 || typeof row.id !== 'string' || !ID.test(row.id)
      || row.id <= previous || !Array.isArray(members) || members.length < 2 || members.length > 8
      || members.some((id, i) => typeof id !== 'string' || !ID.test(id)
        || (i > 0 && members[i - 1] >= id))) return empty(1);
    graph.set(row.id, Object.freeze({ id: row.id, members: Object.freeze([...members] as string[]) }));
    previous = row.id;
  }
  const referenced = new Set<string>();
  for (const row of decoded.rows) {
    if (row.thread === undefined) continue;
    const thread = graph.get(row.thread);
    if (thread === undefined || row.from === row.to || !thread.members.includes(row.from)
      || !thread.members.includes(row.to)) return empty(1);
    referenced.add(row.thread);
  }
  if (referenced.size !== graph.size) return empty(1);
  return { rows: decoded.rows, threads: Object.freeze([...graph.values()]), cut, rejected: 0 };
}

export interface MessageGroup {
  readonly key: string;
  readonly thread?: string;
  readonly participants: readonly string[];
  readonly rows: readonly MessageRow[];
}

export function messageGroups(
  rows: readonly MessageRow[], threads: readonly ThreadRow[], agent: string,
): readonly MessageGroup[] {
  const graph = new Map(threads.map(thread => [thread.id, thread.members]));
  const groups = new Map<string, { key: string; thread?: string; participants: readonly string[]; rows: MessageRow[] }>();
  for (const row of rows) {
    const participants = row.thread === undefined
      ? row.from < row.to ? [row.from, row.to] : [row.to, row.from]
      : graph.get(row.thread);
    if (participants === undefined || !participants.includes(agent)) continue;
    const key = row.thread === undefined ? `pair:${participants.join(':')}` : `thread:${row.thread}`;
    const group = groups.get(key) ?? { key, participants, rows: [],
      ...(row.thread === undefined ? {} : { thread: row.thread }) };
    group.rows.push(row);
    groups.set(key, group);
  }
  return [...groups.values()].map(group => ({ ...group,
    rows: group.rows.sort((a, b) => BigInt(a.lsn) < BigInt(b.lsn) ? -1 : 1),
  })).sort((a, b) => BigInt(a.rows[0].lsn) < BigInt(b.rows[0].lsn) ? -1 : 1);
}
