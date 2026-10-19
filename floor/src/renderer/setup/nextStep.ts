// SPDX-License-Identifier: MIT OR Apache-2.0
//
// What the Floor offers a person who has not given anyone work yet: hire the first agent, install
// the app an agent needs, or type the first task. A pure reading of the confirmed snapshot.

import { fieldsOf } from '../scene/label.ts';
import type { FloorState } from '../floor/store.ts';
import { checksFor } from '../onboarding/checks.ts';
import type { EnrolledCheck } from '../onboarding/checks.ts';
import { HIRE_ENGINES, isHireEngine } from '../onboarding/catalog.ts';
import type { HireEngine } from '../onboarding/catalog.ts';
import { noProjects, tickets, viewConfirmed } from '../ticket/model.ts';
import { decodeWorkspace } from '../workspace.ts';
import { rowFor } from './daemon.ts';

export interface FirstTaskAgent {
  readonly id: string;
  readonly name: string;
  readonly project: string;
  readonly check: EnrolledCheck;
}

export type FirstTaskOffer =
  | { readonly kind: 'hire'; readonly why: 'ready' | 'no_project' | 'no_app' | 'off' }
  | { readonly kind: 'install'; readonly agent: string; readonly engine: HireEngine | null; readonly program: string | null }
  | { readonly kind: 'task'; readonly agents: readonly FirstTaskAgent[] };

/** The title a typed task gives its ticket: its first line, cut to a short length. */
export function titleOf(task: string): string {
  const line = task.trim().split('\n')[0]!.trim();
  return line.length <= 120 ? line : `${line.slice(0, 117)}...`;
}

export function firstTaskOffer(floor: FloorState): FirstTaskOffer | null {
  if (!viewConfirmed(floor)) return null;
  if (floor.agents.length === 0) {
    if (floor.settings?.switches.hire === false) return { kind: 'hire', why: 'off' };
    if (floor.onboarding === null && noProjects(floor)) return { kind: 'hire', why: 'no_project' };
    const apps = floor.daemon?.providers.filter((row) => isHireEngine(row.provider)) ?? [];
    if (apps.length > 0 && apps.every((row) => !row.found)) return { kind: 'hire', why: 'no_app' };
    return { kind: 'hire', why: 'ready' };
  }
  if (!floor.ticket || tickets(floor.cards).some((card) => card.status !== 'done')) return null;
  const free = floor.agents.filter((row) => {
    const fields = fieldsOf(row);
    return (fields.status === 'idle' || fields.status === 'done') && (fields.goal === '' || fields.goalStatus === 'achieved'
      || fields.goalStatus === 'abandoned') && row.retired !== true && row.paused !== true
      && row.halted !== true;
  });
  const agents: FirstTaskAgent[] = [];
  for (const row of free) {
    const project = decodeWorkspace(row.workspace)?.id;
    const check = project === undefined ? undefined : checksFor(floor.checks, project)?.[0];
    if (project !== undefined && check !== undefined) {
      agents.push({ id: row.id, name: typeof row.name === 'string' && row.name !== '' ? row.name : row.id,
        project, check });
    }
  }
  const first = agents[0];
  if (first === undefined) return null;
  const row = floor.agents.find((entry) => entry.id === first.id)!;
  const engine = isHireEngine(row.provider) ? row.provider : null;
  const report = engine === null ? null : rowFor(floor.daemon, engine);
  if (engine !== null && report?.found === false) {
    return { kind: 'install', agent: first.name, engine, program: report.program };
  }
  return { kind: 'task', agents: Object.freeze(agents) };
}

export function appName(engine: HireEngine): string {
  return HIRE_ENGINES[engine];
}
