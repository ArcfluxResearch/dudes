// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useRef } from 'react';

import { ArchivedAgents } from './ArchivedAgents.tsx';
import type { RetiredAgentRow } from './archive.ts';
import { viewButtonId } from './Control.tsx';
import type { FloorState } from './floor/store.ts';
import { JOURNAL_SAVED as SAVED, journalPosition, journalSaved } from './format.ts';
import { restoreFocusAfterDialog } from './focus.ts';
import { StreamReadStatus } from './ReadError.tsx';
import { Roster } from './Roster.tsx';
import { checkoutSummary } from './worktree.ts';

export const OVERVIEW_TITLE_ID = 'overview-title';

/**
 * The top bar's Overview: a dialog over the office with the summary the old view had and the roster in
 * place of its lists. Escape and the close button end it, and focus returns to the Overview button.
 */
export function OverviewDialog({ floor, selectedId, onClose, onSelectAgent, onOpenQuestion, onOpenTicket, onHireAgain }: {
  readonly floor: FloorState;
  readonly selectedId: string | null;
  readonly onClose: () => void;
  readonly onSelectAgent: (id: string) => void;
  readonly onOpenQuestion: (agentId: string) => void;
  readonly onOpenTicket: (card: string) => void;
  readonly onHireAgain?: (row: RetiredAgentRow, opener: HTMLButtonElement) => void;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const selectable = floor.phase === 'live' && !floor.stale;
  const checkouts = checkoutSummary(floor.agents);
  useEffect(() => {
    const node = dialog.current;
    const active = node?.ownerDocument.activeElement;
    const opener = active instanceof HTMLElement && active !== node?.ownerDocument.body ? active : null;
    node?.showModal();
    node?.querySelector<HTMLElement>(`#${OVERVIEW_TITLE_ID}`)?.focus();
    return () => {
      node?.close();
      restoreFocusAfterDialog(opener, node?.ownerDocument.getElementById(viewButtonId('overview')) ?? null);
    };
  }, []);
  return (
    <dialog className="overview-dialog" id="overview-dialog" ref={dialog} aria-labelledby={OVERVIEW_TITLE_ID}
      onCancel={(event) => { event.preventDefault(); onClose(); }}>
      <header className="overview-head">
        <h2 id={OVERVIEW_TITLE_ID} tabIndex={-1}>Overview</h2>
        <button type="button" className="panel-close" aria-label="Close Overview" title="Close (Escape)" onClick={onClose}>×</button>
      </header>
      <div className="overview-body">
        <StreamReadStatus floor={floor} />
        <p className="overview-counts">
          <strong>{floor.agents.length}</strong> {floor.agents.length === 1 ? 'agent' : 'agents'}
          {checkouts === null ? null : <> · {checkouts}</>}
        </p>
        <p className="overview-journal" title={journalSaved(floor.lsn) === SAVED ? `Saved up to ${journalPosition(floor.lsn)}` : undefined}>
          {journalSaved(floor.lsn)}</p>
        <Roster base="overview-roster" agents={floor.agents} cards={floor.cards} stepLabels={floor.stepLabels}
          selectedId={selectedId} selectable={selectable} stale={floor.stale}
          onSelectAgent={onSelectAgent} onOpenQuestion={onOpenQuestion} onOpenTicket={onOpenTicket} />
        <ArchivedAgents floor={floor} onHireAgain={onHireAgain} />
      </div>
    </dialog>
  );
}
