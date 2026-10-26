// SPDX-License-Identifier: MIT OR Apache-2.0

import type { FloorState } from '../floor/store.ts';
import { HINT } from './look.ts';
import { viewConfirmed } from './model.ts';
import { projectAddOn } from '../project/AddProject.tsx';

export function TicketOffHint({ floor }: { readonly floor: FloorState }) {
  if (!viewConfirmed(floor)) return null;
  const needed = (['control', 'goal'] as const).filter((key) => floor.settings?.switches[key] === false);
  const names = needed.map(key => key === 'control' ? 'Pause, resume and rename' : 'Change tasks');
  return <p className={HINT}>Tickets are off. Turn on Make tickets under Floor switches in Settings.
    {names.length === 0 ? null : <> Also turn on {names.join(' and ')}.</>}
  </p>;
}

export function NoTicketProjectHint({ floor }: { readonly floor?: FloorState } = {}) {
  return floor !== undefined && projectAddOn(floor)
    ? <p className={HINT}>Tickets run in a project, and none is set up yet. Add the folder of your project in Settings.</p>
    : <p className={HINT}>Tickets run in a project, and none is set up yet. Turn on Add and remove projects under Floor switches in Settings, then add your project folder.</p>;
}
