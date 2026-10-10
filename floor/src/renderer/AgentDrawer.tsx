// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useState } from 'react';

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
import { GoalForm, GoalStatus } from './goal/GoalPanel.tsx';
import { LiveWork, SelectionBar, streamStatus } from './LiveWork.tsx';
import { MemoryPanel } from './memory/MemoryPanel.tsx';
import { LookStatus } from './look/LookEditor.tsx';
import { MessagesPanel } from './MessagesPanel.tsx';
import { RenameStatus } from './rename/NameEditor.tsx';
import { RetireStatus, RetireStrip } from './retire/RetirePanel.tsx';
import { switchedOffControls, UnavailableControls } from './settings/SettingsView.tsx';
import type { AgentRow } from './scene/cast.ts';
import { TerminalAttach } from './terminal/TerminalPanel.tsx';
import { terminalOffer } from './terminal/terminal.ts';
import { readDetailTab, writeDetailTab } from './view.ts';

/**
 * The results of actions taken in the drawer. They stay on the page when the
 * drawer closes, because a retired agent leaves the selection with its receipt
 * still to read.
 */
export function Receipts({ floor, leaveFocusTo }: { readonly floor: FloorState; readonly leaveFocusTo: string }) {
  return (
    <div className="receipts">
      <PauseAllReceipt leaveFocusTo={leaveFocusTo} />
      <RenameStatus />
      <LookStatus />
      <AnswerStatus floor={floor} leaveFocusTo={leaveFocusTo} />
      <ControlStatus floor={floor} leaveFocusTo={leaveFocusTo} />
      <RetireStatus floor={floor} leaveFocusTo={leaveFocusTo} />
      <GoalStatus floor={floor} leaveFocusTo={leaveFocusTo} />
    </div>
  );
}

export function AgentDrawer({ floor, row, selectedId, onClose, leaveFocusTo }: {
  readonly floor: FloorState;
  readonly row: AgentRow | null;
  readonly selectedId: string | null;
  readonly onClose: () => void;
  readonly leaveFocusTo: string;
}) {
  const chatAvailable = chatGate(floor, row).open;
  const [detailTab, setDetailTab] = useState<DetailTab>(chatAvailable ? 'chat' : 'overview');
  useEffect(() => { setDetailTab(readDetailTab(undefined, chatAvailable ? 'chat' : 'overview')); }, [chatAvailable]);
  useEffect(() => {
    if (floor.phase !== 'live' || floor.stale || floor.snapshotEpoch !== floor.epoch) return;
    const view = detailTab === 'chat' && chatGate(floor, row).open ? 'reply' : 'output';
    if (floor.output.view !== view) selectOutputAgent(floor.output.targetId, false, view);
  }, [detailTab, floor, row]);
  if (row === null && floor.output.targetId === null) return null;
  function chooseDetailTab(tab: DetailTab): void {
    setDetailTab(tab);
    writeDetailTab(tab);
  }
  const selection = <SelectionBar floor={floor} onClear={onClose} leaveFocusTo={leaveFocusTo} showStatus={row === null} />;
  const switchedOff = switchedOffControls(floor);
  return (
    <div className="agent-drawer" id="selected-agent-detail" key={selectedId ?? 'none'}
      ref={handFocusOnLeave} data-focus-on-leave={leaveFocusTo}>
      {row === null ? (
        <header className="agent-detail-head">
          <div className="agent-detail-identity">
            <p className="agent-detail-kicker">Selected agent</p>
            <p>{floor.output.targetId}</p>
          </div>
          {selection}
        </header>
      ) : (
        <AgentDetail row={row} selection={selection} streamState={streamStatus(floor)} tab={detailTab} onTabChange={chooseDetailTab}
          chat={chatGate(floor, row).open ? <ChatPanel floor={floor} row={row} /> : null}
          live={floor.phase === 'live' && !floor.stale && floor.snapshotEpoch === floor.epoch}
          output={floor.output.targetId === row.id && (detailTab === 'output' || floor.output.view === 'reply' || floor.output.chunks.length > 0
            || floor.output.status === 'gap' || floor.output.status === 'unavailable')
            ? <LiveWork floor={floor} row={row} /> : null}
          messages={((floor.messages ?? []).some(message => message.thread === undefined && (message.from === row.id || message.to === row.id))
            || (floor.threads ?? []).some(thread => thread.members.includes(row.id)))
            ? <MessagesPanel messages={floor.messages} threads={floor.threads} agent={row.id}
              agents={floor.agents} retiredAgents={floor.retiredAgents} /> : null}
          terminalDetails={terminalOffer(floor, row, { phase: 'idle' }).kind === 'none'
            ? null : <TerminalAttach floor={floor} row={row} part="details" />}
          git={floor.git ? <GitPanel row={row} active={detailTab === 'git'} /> : null}
          memory={<MemoryPanel agent={row.id} active={detailTab === 'memory'} crew={floor} />}
          control={<ControlStrip floor={floor} row={row} />}
          retire={switchedOff.includes('retire') ? null : <RetireStrip floor={floor} row={row} />}
          terminal={switchedOff.includes('terminal') ? null : <TerminalAttach floor={floor} row={row} part="action" />}
          enable={switchedOff.length === 0 ? null : <details className="enable-controls">
            <summary>Enable more controls <span className="enable-controls-count">{switchedOff.length}</span></summary>
            {switchedOff.map((control) => <UnavailableControls key={control} floor={floor} control={control} />)}
          </details>}
          answer={detailTab === 'chat' && chatGate(floor, row).open ? null
            : <AnswerForm floor={floor} row={row} leaveFocusTo={AGENT_DETAIL_HEADING_ID} />}
          goal={<GoalForm floor={floor} row={row} />} />
      )}
    </div>
  );
}
