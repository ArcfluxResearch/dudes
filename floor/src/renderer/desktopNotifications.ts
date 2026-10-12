// SPDX-License-Identifier: MIT OR Apache-2.0

import { canonicalLsn } from './answer/answer.ts';
import { getFloorState } from './floor/store.ts';
import type { FloorState } from './floor/store.ts';
import type { ProjectionEvent } from './floor/projection.ts';
import { agentRow, tickets, viewConfirmed, waitingTickets } from './ticket/model.ts';
import { readable, reasonSentence } from './ticket/words.ts';
import { readDesktopNotificationChoice, writeDesktopNotificationChoice } from './view.ts';

type NoticeTarget =
  | { readonly kind: 'ticket'; readonly id: string }
  | { readonly kind: 'question'; readonly id: string; readonly questionLsn: string; readonly card: string | null };

export interface DesktopNotice {
  readonly key: string;
  readonly title: string;
  readonly body: string;
  readonly target: NoticeTarget;
}

export interface NotificationHistory {
  readonly epoch: number;
  readonly waiting: ReadonlySet<string>;
}

export type NotificationDestination = { readonly kind: 'ticket' | 'question'; readonly id: string };

function waitingNotices(floor: FloorState): readonly DesktopNotice[] {
  const waiting: DesktopNotice[] = waitingTickets(floor.cards).map((card) => {
    const row = agentRow(floor, card.claimed_by);
    const name = typeof row?.name === 'string' && row.name !== '' ? readable(row.name) : null;
    return {
      key: `ticket:${card.id}:${card.status}`, title: readable(card.title),
      body: card.status === 'review' ? `${name === null ? '' : `${name} finished. `}Ready for you.`
        : `Needs you. ${reasonSentence(card.ticket?.reason ?? null)}`,
      target: { kind: 'ticket', id: card.id },
    };
  });
  for (const row of floor.agents) {
    if (row.status !== 'waiting' || typeof row.question !== 'string' || row.question === ''
      || !canonicalLsn(row.question_lsn)) continue;
    const name = typeof row.name === 'string' && row.name !== '' ? readable(row.name) : 'An agent';
    const card = tickets(floor.cards).find((card) => card.status === 'doing' && card.claimed_by === row.id);
    waiting.push({
      key: `question:${row.id}:${row.question_lsn}`, title: card === undefined ? name : readable(card.title),
      body: `${name} is asking a question.`,
      target: { kind: 'question', id: row.id, questionLsn: row.question_lsn, card: card?.id ?? null },
    });
  }
  return waiting;
}

export function seedNotificationHistory(floor: FloorState): NotificationHistory | null {
  return viewConfirmed(floor) ? { epoch: floor.epoch, waiting: new Set(waitingNotices(floor).map((notice) => notice.key)) } : null;
}

export function notificationArrivals(history: NotificationHistory | null, event: ProjectionEvent): {
  readonly history: NotificationHistory | null;
  readonly notices: readonly DesktopNotice[];
} {
  const floor = event.floor;
  if (!viewConfirmed(floor)) return { history: null, notices: [] };
  const waiting = waitingNotices(floor);
  const next = { epoch: floor.epoch, waiting: new Set(waiting.map((notice) => notice.key)) };
  return {
    history: next,
    notices: event.kind === 'snapshot' || history === null || history.epoch !== floor.epoch ? []
      : waiting.filter((notice) => !history.waiting.has(notice.key)),
  };
}

export function notificationDestination(notice: DesktopNotice, floor: FloorState): NotificationDestination | null {
  if (!viewConfirmed(floor)) return null;
  const target = notice.target;
  if (target.kind === 'ticket') {
    return tickets(floor.cards).some((card) => card.id === target.id) ? { kind: 'ticket', id: target.id } : null;
  }
  const row = agentRow(floor, target.id);
  if (row === null || row.status !== 'waiting' || row.question_lsn !== target.questionLsn
    || typeof row.question !== 'string' || row.question === '') return null;
  const card = tickets(floor.cards).find((card) => card.id === target.card && card.claimed_by === row.id && card.status === 'doing');
  return card === undefined ? { kind: 'question', id: row.id } : { kind: 'ticket', id: card.id };
}

export interface NotificationPreferenceState {
  readonly kind: 'off' | 'on' | 'asking' | 'denied' | 'unsupported' | 'unavailable';
  readonly remembered: boolean;
}

interface NotificationPreferencePorts {
  readChoice(): { readonly enabled: boolean; readonly remembered: boolean };
  writeChoice(enabled: boolean): boolean;
  permission(): NotificationPermission | 'unsupported';
  requestPermission(): Promise<NotificationPermission>;
}

const browserPorts: NotificationPreferencePorts = {
  readChoice: readDesktopNotificationChoice,
  writeChoice: writeDesktopNotificationChoice,
  permission: () => typeof Notification === 'undefined' ? 'unsupported' : Notification.permission,
  requestPermission: () => Notification.requestPermission(),
};

export function createNotificationPreference(ports: NotificationPreferencePorts = browserPorts) {
  let state: NotificationPreferenceState | null = null;
  let request = 0;
  const listeners = new Set<() => void>();

  function getState(): NotificationPreferenceState {
    if (state !== null) return state;
    const { enabled, remembered } = ports.readChoice();
    const permission = ports.permission();
    state = { kind: permission === 'unsupported' ? 'unsupported' : permission === 'denied' ? 'denied'
      : enabled && permission === 'granted' ? 'on' : 'off', remembered };
    return state;
  }

  function publish(kind: NotificationPreferenceState['kind'], remember = false): void {
    let remembered = getState().remembered;
    if (remember) remembered = ports.writeChoice(kind === 'on');
    state = { kind, remembered };
    for (const listener of listeners) listener();
  }

  async function setEnabled(enabled: boolean): Promise<void> {
    const attempt = ++request;
    if (!enabled) { publish('off', true); return; }
    const permission = ports.permission();
    if (permission === 'unsupported' || permission === 'denied') { publish(permission, true); return; }
    if (permission === 'granted') { publish('on', true); return; }
    publish('asking');
    try {
      const result = await ports.requestPermission();
      if (attempt !== request) return;
      publish(result === 'granted' ? 'on' : result === 'denied' ? 'denied' : 'off', true);
    } catch {
      if (attempt === request) publish('unavailable');
    }
  }

  return {
    getState, setEnabled,
    subscribe(listener: () => void): () => void { listeners.add(listener); return () => { listeners.delete(listener); }; },
    unavailable: () => publish('unavailable'),
    permissionChanged: () => {
      const permission = ports.permission();
      if (permission !== 'granted') publish(permission === 'default' ? 'off' : permission);
    },
  };
}

export const desktopNotificationPreference = createNotificationPreference();

export function deliverDesktopNotification(
  notice: DesktopNotice, preference: ReturnType<typeof createNotificationPreference>,
  open: (target: NotificationDestination) => void, floor: () => FloorState = getFloorState,
): boolean {
  if (preference.getState().kind !== 'on' || !viewConfirmed(floor())) return false;
  if (typeof Notification === 'undefined' || Notification.permission !== 'granted') {
    preference.permissionChanged();
    return false;
  }
  try {
    const notification = new Notification(notice.title, { body: notice.body, tag: notice.key });
    notification.addEventListener('click', () => {
      notification.close();
      window.focus();
      const target = notificationDestination(notice, floor());
      if (target !== null) open(target);
    }, { once: true });
    return true;
  } catch {
    preference.unavailable();
    return false;
  }
}
