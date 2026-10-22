// SPDX-License-Identifier: MIT OR Apache-2.0

import type { FloorState } from './floor/store.ts';
import { decodeWorktree, worktreeStateText } from './worktree.ts';
import type { Worktree } from './worktree.ts';

/** The agents that work in their own git worktree, read only. Removing a worktree stays a command-line job. */
export function WorkersList({ agents }: { readonly agents: FloorState['agents'] }) {
  const workers = agents.flatMap((row) => {
    const worktree = decodeWorktree(row.worktree);
    return worktree === null || worktree.state === 'removed' ? [] : [{ id: row.id, name: typeof row.name === 'string' && row.name !== '' ? row.name : row.id, worktree }];
  });
  if (workers.length === 0) return null;
  return (
    <section className="workers-list" aria-labelledby="workers-title">
      <h3 id="workers-title">Workers <span>{workers.length}</span></h3>
      <p>Agents that work on their own branch in their own worktree. This list only reports them.</p>
      <ul>
        {workers.map(({ id, name, worktree }) => (
          <li key={id} data-worker-id={id}>
            <strong><bdi>{name}</bdi></strong>
            <dl>
              <div><dt>Branch</dt><dd className="data">{worktree.branch}</dd></div>
              <div><dt>State</dt><dd>{worktreeStateText(worktree.state)}</dd></div>
              <div><dt>Started from</dt><dd>{baseText(worktree)}</dd></div>
            </dl>
          </li>
        ))}
      </ul>
    </section>
  );
}

function baseText(worktree: Worktree) {
  return worktree.from === undefined
    ? "The project's current commit when the agent was hired"
    : <>Another agent&apos;s worktree <span className="data">{worktree.from}</span></>;
}
