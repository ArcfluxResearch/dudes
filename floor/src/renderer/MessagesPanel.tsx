// SPDX-License-Identifier: MIT OR Apache-2.0

import { Badge, Card, Disclosure, cn } from '../ui/index.ts';
import type { BadgeState } from '../ui/index.ts';
import { RelativeTime } from './RelativeTime.tsx';
import type { RetiredAgentRow } from './archive.ts';
import { messageGroups } from './messages.ts';
import type { MessageRow, ThreadRow } from './messages.ts';
import type { AgentRow } from './scene/cast.ts';

const MESSAGE_STATE: Readonly<Record<MessageRow['state'], string>> = {
  durable: 'Saved', notified: 'Notified', acked: 'Acknowledged', bounced: 'Delivery failed',
};

const REPLY_STATUS: Readonly<Record<MessageRow['reply_status'], { readonly label: string; readonly state: BadgeState }>> = {
  not_expected: { label: '', state: 'idle' },
  answered: { label: 'Answered', state: 'done' },
  undeliverable: { label: 'Undeliverable', state: 'needs-you' },
  thread_full: { label: 'Thread full', state: 'needs-you' },
  awaiting: { label: 'Awaiting reply', state: 'waiting' },
};

const ACT_STATE: Readonly<Record<NonNullable<MessageRow['act']>, BadgeState>> = {
  message: 'idle', request: 'working',
};

export function MessagesPanel({ messages, threads, agent, agents, retiredAgents }: {
  readonly messages: readonly MessageRow[];
  readonly threads: readonly ThreadRow[];
  readonly agent: string;
  readonly agents: readonly AgentRow[];
  readonly retiredAgents: readonly RetiredAgentRow[];
}) {
  const names = new Map<string, string>(retiredAgents.map(row => [row.id, row.name]));
  for (const row of agents) {
    if (typeof row.name === 'string' && row.name !== '') names.set(row.id, row.name);
  }
  const name = (id: string) => names.get(id) || id;
  const groups = messageGroups(messages, threads, agent);
  return <div className={cn('flex min-w-0 flex-col gap-4 font-sans text-base leading-5 text-ink')}>
    <p className={cn('m-0 text-small text-muted [overflow-wrap:anywhere]')}>
      Recent agent messages. Older messages may be outside this window.
    </p>
    {groups.length === 0 && <Card className={cn('border-dashed border-line-strong p-4 shadow-none')}>
      <p className={cn('m-0 text-base text-muted [overflow-wrap:anywhere]')}>No recent messages.</p>
    </Card>}
    {groups.map(pair => <section key={pair.key} className={cn('m-0 min-w-0')}
      data-message-pair={pair.thread === undefined ? pair.participants.join(':') : undefined}
      data-message-thread={pair.thread}>
      <h3 className={cn('m-0 mb-2 font-sans text-base font-semibold leading-5 text-ink [overflow-wrap:anywhere]')}>
        {pair.participants.map(name).join(' · ')}
      </h3>
      <ol className={cn('m-0 flex list-none flex-col gap-3 p-0')}
        aria-label={`Messages between ${pair.participants.map(name).join(', ')}`}>
        {pair.rows.map(row => <li key={row.id} data-message-id={row.id} className={cn('min-w-0 border-0 p-0')}>
          <Card className={cn('flex min-w-0 flex-col gap-2 p-3')}>
            {row.subject !== null && row.subject !== '' && <h4 data-message-subject
              className={cn('m-0 line-clamp-2 font-sans text-body font-semibold leading-5 text-ink [overflow-wrap:anywhere]')}>
              <bdi>{row.subject}</bdi>
            </h4>}
            <p className={cn('m-0 text-base [overflow-wrap:anywhere]')}>
              <strong className={cn('font-medium')}>{name(row.from)}</strong>{' to '}
              <strong className={cn('font-medium')}>{name(row.to)}</strong>
            </p>
            <p className={cn('m-0 text-small text-muted [overflow-wrap:anywhere]')}>
              <RelativeTime at={row.at} /> · <span>{MESSAGE_STATE[row.state]}</span>
            </p>
            {(row.act !== null || row.reply_status !== 'not_expected') && <div className={cn('flex flex-wrap items-center gap-2')}>
              {row.act !== null && <span className={cn('inline-flex flex-wrap items-center gap-1.5 text-small text-muted')}>
                <span>Sender marked this as </span><Badge state={ACT_STATE[row.act]}>{row.act}</Badge>
              </span>}
              {row.reply_status !== 'not_expected' && <span data-reply-status={row.reply_status}>
                <Badge state={REPLY_STATUS[row.reply_status].state}>{REPLY_STATUS[row.reply_status].label}</Badge>
              </span>}
            </div>}
            <Disclosure summary="Details" className={cn('border-t border-line pt-1')}
              summaryClassName={cn('text-small text-muted')}>
              <dl className={cn('m-0 grid min-w-0 gap-2 text-small leading-5')}>
                <div className={cn('min-w-0 border-0 p-0')}>
                  <dt className={cn('text-small font-medium tracking-[normal] text-muted normal-case')}>Message id</dt>
                  <dd className={cn('m-0 font-mono text-small text-ink [overflow-wrap:anywhere]')}>{row.id}</dd>
                </div>
                <div className={cn('min-w-0 border-0 p-0')}>
                  <dt className={cn('text-small font-medium tracking-[normal] text-muted normal-case')}>Delivery state</dt>
                  <dd className={cn('m-0 text-small text-ink [overflow-wrap:anywhere]')}>{row.state}</dd>
                </div>
                <div className={cn('min-w-0 border-0 p-0')}>
                  <dt className={cn('text-small font-medium tracking-[normal] text-muted normal-case')}>Creation order</dt>
                  <dd className={cn('m-0 font-mono text-small text-ink [overflow-wrap:anywhere]')}>{row.lsn}</dd>
                </div>
              </dl>
            </Disclosure>
          </Card>
        </li>)}
      </ol>
    </section>)}
  </div>;
}
