// SPDX-License-Identifier: MIT OR Apache-2.0

import type { FloorState } from '../floor/store.ts';
import { isHireEngine } from '../onboarding/catalog.ts';

export type GuideStep = 'welcome' | 'engines' | 'workspaces' | 'actions' | 'hire';
export type GuideStatus = 'Done' | 'Not done' | 'Optional';
const STEPS: readonly { readonly key: GuideStep; readonly label: string; readonly optional?: true;
  readonly ready: (floor: FloorState, actionsSeen: boolean) => boolean }[] = [
  { key: 'welcome', label: 'Welcome', ready: () => true },
  { key: 'engines', label: 'Engines', ready: (floor) => floor.daemon?.providers.some(
    (row) => isHireEngine(row.provider) && row.found) ?? false },
  { key: 'workspaces', label: 'Projects', ready: (floor) => (floor.settings?.workspaces.length ?? 0) > 0 },
  { key: 'actions', label: 'Floor actions', optional: true, ready: (_floor, actionsSeen) => actionsSeen },
  { key: 'hire', label: 'First hire', ready: (floor) => floor.agents.length > 0 },
];

export function guideSnapshotConfirmed(floor: FloorState): boolean {
  return floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch;
}

export function guideSteps(floor: FloorState, actionsSeen: boolean):
  readonly { key: GuideStep; label: string; status: GuideStatus }[] {
  const confirmed = guideSnapshotConfirmed(floor);
  return STEPS.map(({ key, label, optional, ready }) => ({ key, label,
    status: confirmed && ready(floor, actionsSeen) ? 'Done' : optional ? 'Optional' : 'Not done' }));
}
