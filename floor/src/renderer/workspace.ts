// SPDX-License-Identifier: MIT OR Apache-2.0

import type { AgentRow } from './scene/cast.ts';

export interface Workspace {
  readonly id: string;
  readonly label: string;
}

export interface WorkspaceGroup {
  readonly id: string | null;
  readonly label: string;
  readonly labelsDiffer: boolean;
  readonly agents: readonly AgentRow[];
}

export function decodeWorkspace(value: unknown): Workspace | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  const { id, label } = value as Record<string, unknown>;
  if (typeof id !== 'string' || !/^[a-zA-Z0-9_-]{1,128}$/.test(id)
    || typeof label !== 'string' || label.length === 0
    || new TextEncoder().encode(label).length > 128 || /\p{Cc}/u.test(label)) return null;
  return Object.freeze({ id, label });
}

export function workspaceLabel(row: AgentRow): string {
  return decodeWorkspace(row.workspace)?.label ?? 'Project not recorded';
}

export function workspaceGroups(agents: readonly AgentRow[]): readonly WorkspaceGroup[] {
  const grouped = new Map<string | null, { labels: Set<string>; agents: AgentRow[] }>();
  for (const row of agents) {
    const workspace = decodeWorkspace(row.workspace);
    const id = workspace?.id ?? null;
    let group = grouped.get(id);
    if (group === undefined) {
      group = { labels: new Set(), agents: [] };
      grouped.set(id, group);
    }
    if (workspace !== null) group.labels.add(workspace.label);
    group.agents.push(row);
  }
  return [...grouped.entries()]
    .sort(([a], [b]) => a === b ? 0 : a === null ? 1 : b === null ? -1 : a < b ? -1 : 1)
    .map(([id, group]) => ({
      id,
      label: [...group.labels].sort()[0] ?? 'Project not recorded',
      labelsDiffer: group.labels.size > 1,
      agents: group.agents,
    }));
}
