// SPDX-License-Identifier: MIT OR Apache-2.0

import { Badge, cn } from '../../ui/index.ts';
import type { BadgeState } from '../../ui/index.ts';
import type { FloorState } from '../floor/store.ts';
import type { AskedRequest } from './asked.ts';
import { shownSubject } from './model.ts';

const STATUS: Readonly<Record<AskedRequest['status'], { readonly label: string; readonly state: BadgeState }>> = {
  awaiting: { label: 'Waiting', state: 'waiting' },
  answered: { label: 'Answered', state: 'done' },
  undeliverable: { label: 'Not delivered', state: 'needs-you' },
  thread_full: { label: 'Thread full', state: 'needs-you' },
};

export function agentNames(floor: FloorState): ReadonlyMap<string, string> {
  const names = new Map<string, string>(floor.retiredAgents.map(row => [row.id, row.name]));
  for (const row of floor.agents) {
    if (typeof row.name === 'string' && row.name !== '') names.set(row.id, row.name);
  }
  return names;
}

/** One quiet line in the conversation for a request the agent sent to a teammate. */
export function AskedRow({ request, from, to, onOpenMail }: {
  readonly request: AskedRequest; readonly from: string; readonly to: string;
  readonly onOpenMail?: (id: string) => void;
}) {
  const status = STATUS[request.status];
  const subject = shownSubject(request.subject);
  const sentence = subject === null ? `${from} asked ${to} a question` : `${from} asked ${to}: ${subject}`;
  const body = <>
    <span className={cn('min-w-0 flex-1 truncate')}>{subject === null ? sentence : <>{from} asked {to}: <bdi data-asked-subject>{subject}</bdi></>}</span>
    <Badge state={status.state}>{status.label}</Badge>
  </>;
  const look = 'flex w-full min-w-0 items-center gap-2 text-start text-small text-muted';
  return <li data-asked-request={request.id} data-asked-status={request.status} className="asked-row m-0 min-w-0 list-none">
    {onOpenMail === undefined
      ? <div className={look} title={sentence}>{body}</div>
      : <button type="button" className={cn(look, 'asked-open cursor-pointer rounded-md border-0 bg-transparent p-0 hover:text-ink focus-visible:text-ink')}
        title={sentence} aria-label={`${sentence}. ${status.label}. ${request.replied ? 'See reply' : 'See message'}`}
        onClick={() => onOpenMail(request.target)}>{body}</button>}
  </li>;
}
