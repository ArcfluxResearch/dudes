// SPDX-License-Identifier: MIT OR Apache-2.0

import { Badge, Button, Disclosure, cn } from '../../ui/index.ts';
import type { BadgeState } from '../../ui/index.ts';
import { RelativeTime } from '../RelativeTime.tsx';
import type { FloorState } from '../floor/store.ts';
import { askedRequests } from './asked.ts';
import type { AskedRequest } from './asked.ts';
import { NOTE } from './look.ts';

const STATUS: Readonly<Record<AskedRequest['status'], { readonly label: string; readonly state: BadgeState }>> = {
  awaiting: { label: 'Waiting', state: 'waiting' },
  answered: { label: 'Answered', state: 'done' },
  undeliverable: { label: 'Not delivered', state: 'needs-you' },
  thread_full: { label: 'Thread full', state: 'needs-you' },
};

export function AskedRequests({ floor, agent, onOpenMail }: {
  readonly floor: FloorState; readonly agent: string; readonly onOpenMail?: (id: string) => void;
}) {
  const asked = askedRequests(floor.messages ?? [], floor.threads ?? [], agent);
  if (asked.length === 0) return null;
  const names = new Map<string, string>(floor.retiredAgents.map(row => [row.id, row.name]));
  for (const row of floor.agents) {
    if (typeof row.name === 'string' && row.name !== '') names.set(row.id, row.name);
  }
  const answered = asked.filter(request => request.status === 'answered');
  const latest = answered.at(-1);
  const older = answered.slice(0, -1);
  const open = asked.filter(request => request.status !== 'answered' || request === latest);
  const card = (request: AskedRequest) => {
    const status = STATUS[request.status];
    return <li key={request.id} data-asked-request={request.id} data-asked-status={request.status}
      className={cn('flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 rounded-md border border-line bg-surface px-2 py-1.5 text-small')}>
      <span className={cn('min-w-0 flex-1 [overflow-wrap:anywhere]')}>
        <span className={cn('font-medium')}>Asked {names.get(request.to) || request.to}</span>
        {request.subject !== null && request.subject !== '' ? <>{': '}<bdi data-asked-subject>{request.subject}</bdi></> : null}
      </span>
      <Badge state={status.state}>{status.label}</Badge>
      <RelativeTime at={request.at} className={NOTE} />
      {onOpenMail === undefined ? null : <Button size="sm" variant="ghost" className="asked-open"
        onClick={() => onOpenMail(request.target)}>{request.replied ? 'See reply' : 'See message'}</Button>}
    </li>;
  };
  return <section className={cn('asked-requests flex min-w-0 flex-col gap-1.5')} aria-label="Requests this agent sent">
    <ul className={cn('m-0 flex list-none flex-col gap-1.5 p-0')}>{open.map(card)}</ul>
    {older.length === 0 ? null : <Disclosure summaryClassName="py-0 text-small font-normal text-muted"
      summary={`Earlier answered requests (${older.length})`}>
      <ul className={cn('m-0 mt-1.5 flex list-none flex-col gap-1.5 p-0')}>{older.map(card)}</ul>
    </Disclosure>}
  </section>;
}
