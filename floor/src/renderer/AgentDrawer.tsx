// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useState } from 'react';

import { Disclosure } from '../ui/index.ts';
import { AgentDetail } from './AgentDetail.tsx';
import { AnswerForm, AnswerStatus } from './answer/AnswerPanel.tsx';
import { ControlStatus, ControlStrip } from './control/ControlPanel.tsx';
import { ChatPanel } from './chat/ChatPanel.tsx';
import { chatGate } from './chat/chat.ts';
import { PauseAllReceipt } from './control/PauseAllControl.tsx';
import type { DetailTab } from './detailTabs.ts';
import { selectOutputAgent } from './floor/store.ts';
import type { FloorState } from './floor/store.ts';
import { AGENT_DETAIL_HEADING_ID, handFocusOnLeave } from './focus.ts';
import { GitPanel } from './git/GitPanel.tsx';
import { goalGate } from './goal/goal.ts';
import { FirstTaskForm, ticketActionsOpen } from './ticket/TicketBoard.tsx';
import { freeTaskAgents } from './setup/nextStep.ts';
import { GoalForm, GoalStatus } from './goal/GoalPanel.tsx';
import { OverviewOutput, SelectionBar, streamStatus } from './LiveWork.tsx';
import { MemoryPanel } from './memory/MemoryPanel.tsx';
import { LookStatus } from './look/LookEditor.tsx';
import { ModelStatus } from './model/ModelEditor.tsx';
import { MessagesPanel } from './MessagesPanel.tsx';
import { RenameStatus } from './rename/NameEditor.tsx';
import { RetireMenuItem, RetireStatus } from './retire/RetirePanel.tsx';
import { automaticHoldOf } from './automaticHold.ts';
import { RESUME_SWITCH } from './ticket/stuck.ts';
import { RecoveryPanel, RecoveryReceipts } from './recovery/RecoveryPanel.tsx';
import { switchedOffControls, UnavailableControls } from './settings/SettingsView.tsx';
import type { AgentRow } from './scene/cast.ts';
import { TerminalAttach } from './terminal/TerminalPanel.tsx';
import { readDetailTab, writeDetailTab } from './view.ts';
import { Notice, cn } from '../ui/index.ts';

/**
 * The results of actions taken in the drawer. They stay on the page when the
 * drawer closes, because a retired agent leaves the selection with its receipt
 * still to read.
 */
export function Receipts({ floor, leaveFocusTo, focused = false }: {
  readonly floor: FloorState; readonly leaveFocusTo: string; readonly focused?: boolean;
}) {
  return (
    <div className={cn('receipts grid max-h-[40%] w-[min(var(--side-width),45vw)] flex-none gap-2 overflow-auto pr-2 pt-2 empty:hidden',
      'has-[[class~=recovery-receipt]:focus-within]:max-h-[calc(100dvh-8rem)] max-[800px]:m-2 max-[800px]:w-auto max-[800px]:p-0', focused && 'p-0')}>
      <PauseAllReceipt leaveFocusTo={leaveFocusTo} />
      <RenameStatus />
      <LookStatus />
      <ModelStatus />
      <AnswerStatus floor={floor} leaveFocusTo={leaveFocusTo} />
      <ControlStatus floor={floor} leaveFocusTo={leaveFocusTo} />
      <RetireStatus floor={floor} leaveFocusTo={leaveFocusTo} />
      <GoalStatus floor={floor} leaveFocusTo={leaveFocusTo} />
      <RecoveryReceipts floor={floor} />
    </div>
  );
}

const DRAWER = cn('agent-drawer m-0 box-border flex min-w-0 flex-col overflow-hidden border-0 border-solid border-line bg-surface text-ink',
  'max-[800px]:not-has-[[data-detail-tab=chat]]:top-[var(--banner-height,0px)]',
  '[&_[class~=agent-detail]]:border-0 [&_[class~=agent-detail]]:bg-transparent [&_[class~=agent-detail]]:p-0 [&_[class~=agent-detail]]:shadow-none',
  '[&_[class~=agent-detail-identity]]:pr-8');
const DOCKED = 'min-[801px]:relative min-[801px]:z-(--layer-panels) min-[801px]:w-[min(var(--side-width),45vw)] min-[801px]:flex-none min-[801px]:border-l min-[801px]:p-4';
const IN_FOCUS = 'min-[801px]:relative min-[801px]:z-(--layer-panels) min-[801px]:w-auto min-[801px]:flex-none min-[801px]:rounded-[var(--radius)] min-[801px]:border min-[801px]:p-4';

export function AgentDrawer({ floor, row, selectedId, inFocus = false, onClose, leaveFocusTo, recoveryRequested, onRecoveryShown, onReadRecoveryReceipt, onOpenTicket }: {
  readonly floor: FloorState;
  readonly row: AgentRow | null;
  readonly selectedId: string | null;
  /** True inside focus mode's frame, where the drawer fills its column and keeps a full edge. */
  readonly inFocus?: boolean;
  readonly onClose: () => void;
  readonly leaveFocusTo: string;
  readonly recoveryRequested?: { readonly id: string } | null;
  readonly onRecoveryShown?: () => void;
  readonly onReadRecoveryReceipt?: (request: string) => void;
  readonly onOpenTicket?: (id: string) => void;
}) {
  const chatAvailable = chatGate(floor, row).open;
  const [detailTab, setDetailTab] = useState<DetailTab>(chatAvailable ? 'chat' : 'overview');
  useEffect(() => { setDetailTab(readDetailTab(undefined, chatAvailable ? 'chat' : 'overview')); }, [chatAvailable]);
  useEffect(() => {
    if (recoveryRequested?.id === row?.id && row !== null) setDetailTab('overview');
  }, [recoveryRequested, row?.id]);
  useEffect(() => {
    if (recoveryRequested?.id !== row?.id || row === null || detailTab !== 'overview') return;
    const heading = document.getElementById('recovery-title');
    let parent = heading?.parentElement;
    while (parent) {
      if (parent.tagName === 'DETAILS') (parent as HTMLDetailsElement).open = true;
      parent = parent.parentElement;
    }
    heading?.focus();
    heading?.scrollIntoView({ block: 'start' });
    onRecoveryShown?.();
  }, [recoveryRequested, row?.id, detailTab, onRecoveryShown]);
  const [fullOutput, setFullOutput] = useState(false);
  useEffect(() => { setFullOutput(false); }, [selectedId]);
  useEffect(() => {
    if (floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return;
    const view = detailTab === 'chat' && chatGate(floor, row).open && !fullOutput ? 'reply' : 'output';
    if (floor.output.view !== view) selectOutputAgent(floor.output.targetId, false, view);
  }, [detailTab, floor, row, fullOutput]);
  if (row === null && floor.output.targetId === null) return null;
  function chooseDetailTab(tab: DetailTab): void {
    setDetailTab(tab);
    writeDetailTab(tab);
  }
  const selection = <SelectionBar floor={floor} onClear={onClose} leaveFocusTo={leaveFocusTo} showStatus={row === null} />;
  const switchedOff = switchedOffControls(floor).filter((control) => control !== 'terminal');
  const attachOff = switchedOffControls(floor).includes('terminal');
  const taskAgents = ticketActionsOpen(floor) && row !== null
    ? freeTaskAgents(floor).filter((agent) => agent.id === row.id) : [];
  const changeGoal = row !== null && goalGate(floor, row).open;
  return (
    <div className={cn(DRAWER, inFocus ? IN_FOCUS : DOCKED)} id="selected-agent-detail" key={selectedId ?? 'none'}
      ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}>
      {row === null ? (
        <header className="agent-detail-head grid grid-cols-1 gap-3 font-sans text-ink">
          <div className="agent-detail-identity grid gap-0.5">
            <p className="agent-detail-kicker m-0 text-caption font-semibold uppercase tracking-label text-muted">Selected agent</p>
            <p className="m-0 text-body">{floor.output.targetId}</p>
          </div>
          {selection}
        </header>
      ) : (
        <AgentDetail row={row} cards={floor.cards} selection={selection} streamState={streamStatus(floor)} tab={detailTab} onTabChange={chooseDetailTab}
          chat={chatGate(floor, row).open
            ? <ChatPanel floor={floor} row={row} fullOutput={fullOutput} onFullOutput={setFullOutput} /> : null}
          liveOutput={floor.output.targetId === row.id && (floor.output.chunks.length > 0
            || floor.output.status === 'gap' || floor.output.status === 'unavailable')
            ? <OverviewOutput floor={floor} row={row} /> : null}
          live={floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch}
          messages={((floor.messages ?? []).some(message => message.thread === undefined && (message.from === row.id || message.to === row.id))
            || (floor.threads ?? []).some(thread => thread.members.includes(row.id)))
            ? <MessagesPanel messages={floor.messages} threads={floor.threads} agent={row.id}
              agents={floor.agents} retiredAgents={floor.retiredAgents} /> : null}
          git={floor.git ? <GitPanel row={row} active={detailTab === 'git'} /> : null}
          memory={<MemoryPanel agent={row.id} active={detailTab === 'memory'} crew={floor} />}
          control={floor.settings && !floor.control && automaticHoldOf(row) !== null
            ? <Notice tone="warn">Resume is off. Turn on {RESUME_SWITCH} under Floor switches in Settings.</Notice>
            : <ControlStrip floor={floor} row={row} />}
          recovery={<RecoveryPanel floor={floor} row={row} onReadReceipt={onReadRecoveryReceipt} />}
          retire={switchedOff.includes('retire') ? null : <RetireMenuItem floor={floor} row={row} />}
          terminal={attachOff ? null : <TerminalAttach floor={floor} row={row} />}
          attachNote={attachOff && row.provider === 'claude'
            ? <p className="agent-attach-note m-0 text-small text-muted">To open the real CLI, turn on Attach terminals in Settings.</p> : null}
          enable={switchedOff.length === 0 ? null : <Disclosure className="enable-controls"
            summary={<>Enable more controls <span className="enable-controls-count text-small text-subtle">{switchedOff.length}</span></>}>
            {switchedOff.map((control) => <UnavailableControls key={control} floor={floor} control={control} />)}
          </Disclosure>}
          answer={detailTab === 'chat' && chatGate(floor, row).open ? null
            : <AnswerForm floor={floor} row={row} leaveFocusTo={AGENT_DETAIL_HEADING_ID} />}
          onOpenTicket={onOpenTicket}
          task={taskAgents.length === 0 ? null : <FirstTaskForm floor={floor} agents={taskAgents} />}
          goal={changeGoal ? <GoalForm floor={floor} row={row} /> : null} />
      )}
    </div>
  );
}
