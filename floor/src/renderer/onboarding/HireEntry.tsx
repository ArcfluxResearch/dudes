// SPDX-License-Identifier: MIT OR Apache-2.0

import { Button, Disclosure } from '../../ui/index.ts';
import type { FloorState } from '../floor/store.ts';
import { hireUnavailable } from '../settings/SettingsView.tsx';

export function hireSwitchOn(floor: FloorState): boolean {
  return floor.settings?.switches.hire ?? floor.onboarding !== null;
}

/** Add agent, or why Hire is not offered. `folded` puts the reason behind a one-line disclosure. */
export function HireEntry({ floor, id, onAddAgent, folded = false }: {
  readonly floor: FloorState;
  readonly id: string;
  readonly onAddAgent: (opener: HTMLButtonElement) => void;
  readonly folded?: boolean;
}) {
  if (!hireSwitchOn(floor)) {
    const reason = <p data-hire-entry="off" className="m-0 text-small leading-normal text-muted">{hireUnavailable(floor)}</p>;
    if (!folded) return reason;
    return <Disclosure data-hire-entry="folded" summary={floor.settings?.switches.hire === false ? 'Hire is off' : 'Hire is unavailable'}
      className="mt-auto px-1 pt-2 text-small" summaryClassName="text-small text-muted">
      {reason}
    </Disclosure>;
  }
  return <Button id={id} data-hire-entry="add"
    onClick={(event) => onAddAgent(event.currentTarget)}>Add agent</Button>;
}
