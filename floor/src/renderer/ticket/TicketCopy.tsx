// SPDX-License-Identifier: MIT OR Apache-2.0

import type { CardRow } from '../board.ts';
import type { FloorState } from '../floor/store.ts';
import { Button } from '../../ui/index.ts';
import { copyOf, copyState, earlierRoundLine, SHARED_LINE } from './copy.ts';
import type { TicketHistory } from './history.ts';

/** Bring the ticket's Changes view into sight. */
function showChangesView(): void {
  document.querySelector('.diff-panel')?.scrollIntoView({ block: 'start' });
}

/** The branch chip on a Board card: the branch of the copy an agent is working in. */
export function BranchChip({ floor, card }: { readonly floor: FloorState; readonly card: CardRow }) {
  if (card.status !== 'doing' && card.status !== 'review') return null;
  const copy = copyOf(floor, card);
  if (copy === null || copy.kind !== 'own' || copy.worktree.state === 'removed') return null;
  return <span className="ticket-branch min-w-0 truncate rounded-sm bg-surface-2 px-1.5 py-0.5 font-mono text-caption text-muted"
    title="The agent works in its own copy of the project, on this branch.">{copy.worktree.branch}</span>;
}

/** Where this ticket's agent works: its branch and copy state with links, or one line saying it shares the folder. */
export function TicketCopy({ floor, card, history, showChanges, onOpenGit }: {
  readonly floor: FloorState;
  readonly card: CardRow;
  readonly history: TicketHistory | null;
  /** The ticket page shows its Changes view. */
  readonly showChanges: boolean;
  readonly onOpenGit?: (agent: string) => void;
}) {
  const copy = copyOf(floor, card);
  if (copy === null || card.claimed_by === null) return null;
  const earlier = earlierRoundLine(floor, card, history);
  const agent = card.claimed_by;
  if (copy.kind === 'shared') return <section aria-label="Where it works" className="ticket-copy m-0">
    <p className="m-0 text-muted">{SHARED_LINE}</p>
  </section>;
  const { worktree } = copy;
  return <section aria-label="Where it works" className="ticket-copy flex flex-col gap-2">
    <dl className="m-0 flex flex-wrap items-baseline gap-x-6 gap-y-1">
      <div><dt className="text-caption font-semibold tracking-label text-muted uppercase">Branch</dt>
        <dd className="m-0 mt-0.5 font-mono text-small wrap-anywhere" data-ticket-branch>{worktree.branch}</dd></div>
      <div><dt className="text-caption font-semibold tracking-label text-muted uppercase">Its own copy</dt>
        <dd className="m-0 mt-0.5" data-ticket-copy-state={worktree.state}>{copyState(worktree)}</dd></div>
    </dl>
    {worktree.state === 'removed' ? null : <div className="flex flex-wrap gap-2">
      {showChanges ? <Button size="sm" className="ticket-copy-diff"
        onClick={showChangesView}>See this copy’s changes</Button> : null}
      {floor.git && onOpenGit !== undefined ? <Button size="sm" className="ticket-copy-git"
        onClick={() => onOpenGit(agent)}>Open the agent’s Git view</Button> : null}
    </div>}
    {earlier === null ? null : <p className="ticket-earlier m-0 text-muted">{earlier}</p>}
  </section>;
}
