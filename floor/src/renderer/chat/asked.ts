// SPDX-License-Identifier: MIT OR Apache-2.0

import type { MessageRow, ThreadRow } from '../messages.ts';

export interface AskedRequest {
  readonly id: string;
  readonly to: string;
  readonly at: number;
  readonly lsn: string;
  readonly subject: string | null;
  readonly status: Exclude<MessageRow['reply_status'], 'not_expected'>;
  readonly target: string;
  readonly replied: boolean;
}

/** The direct requests `agent` sent to one teammate, oldest first, each with the mail to jump to. */
export function askedRequests(
  rows: readonly MessageRow[], threads: readonly ThreadRow[], agent: string,
): readonly AskedRequest[] {
  const members = new Map(threads.map(thread => [thread.id, thread.members]));
  const sameConversation = (a: MessageRow, b: MessageRow) =>
    a.thread === undefined ? b.thread === undefined : a.thread === b.thread;
  const asked: AskedRequest[] = [];
  for (const row of rows) {
    if (row.act !== 'request' || row.from !== agent || row.reply_status === 'not_expected') continue;
    if (row.thread !== undefined && members.get(row.thread)?.length !== 2) continue;
    const answer = row.reply_status === 'answered'
      ? rows.find(other => sameConversation(row, other) && other.from === row.to && other.to === agent
        && other.state !== 'bounced' && (other.act === 'message' || other.act === 'request')
        && BigInt(other.lsn) > BigInt(row.lsn))
      : undefined;
    asked.push({ id: row.id, to: row.to, at: row.at, lsn: row.lsn, subject: row.subject, status: row.reply_status,
      target: answer?.id ?? row.id, replied: answer !== undefined });
  }
  return asked;
}

/**
 * Where each request sits among the conversation's exchanges. Mail and chat notes share one journal order,
 * so a request follows the last exchange whose note came before it. A request older than every exchange
 * leads the conversation.
 */
export function placeAsked(
  noteLsns: readonly string[], asked: readonly AskedRequest[],
): { readonly leading: readonly AskedRequest[]; readonly after: ReadonlyMap<string, readonly AskedRequest[]> } {
  const leading: AskedRequest[] = [];
  const after = new Map<string, AskedRequest[]>();
  for (const request of asked) {
    let anchor: string | undefined;
    for (const lsn of noteLsns) if (BigInt(lsn) < BigInt(request.lsn)) anchor = lsn;
    if (anchor === undefined) leading.push(request);
    else after.set(anchor, [...after.get(anchor) ?? [], request]);
  }
  return { leading, after };
}
