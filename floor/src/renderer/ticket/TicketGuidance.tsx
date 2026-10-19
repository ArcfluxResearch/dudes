// SPDX-License-Identifier: MIT OR Apache-2.0

import type { FloorState } from '../floor/store.ts';
import { viewConfirmed } from './model.ts';

export function TicketOffHint({ floor }: { readonly floor: FloorState }) {
  if (!viewConfirmed(floor)) return null;
  const needed = (['control', 'goal'] as const).filter((key) => floor.settings?.switches[key] === false);
  const command = `dudes config set floor.board on${needed.map((key) => ` floor.${key} on`).join('')}`;
  return <p className="ticket-hint">Tickets are off. Turn them on with <code>{command}</code>, then restart the daemon.
    {needed.length === 0 ? null : <> Tickets also need {needed.map((key, at) => <span key={key}>
      {at === 0 ? '' : ' and '}<code>floor.{key}</code>
    </span>)} on.</>}
  </p>;
}

export function NoTicketProjectHint() {
  return <p className="ticket-hint">Tickets run in a project, and none is set up yet. In your project&apos;s folder run <code>dudes project add</code>.</p>;
}
