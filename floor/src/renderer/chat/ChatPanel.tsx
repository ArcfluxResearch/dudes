// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useLayoutEffect, useRef, useSyncExternalStore } from 'react';
import { answerGate, canonicalLsn } from '../answer/answer.ts';
import { AnswerForm } from '../answer/AnswerPanel.tsx';
import { selectOutputAgent } from '../floor/store.ts';
import type { FloorState, OutputStatus } from '../floor/store.ts';
import { handFocusOnLeave } from '../focus.ts';
import type { AgentRow } from '../scene/cast.ts';
import { browserChatDeps, cancelChat, chatDraft, chatGate, chatReleasable, chatSettled, editChatDraft,
  getChatState, noteProblem, queuesChatOnKey, reconcileChat, releaseChat, retryChat, selectChatAnswer, startFreshChat, submitChat, subscribeToChat } from './chat.ts';
import type { ChatState } from './chat.ts';
import type { ChatReason, ChatTurn, PendingNote } from './model.ts';
import { conversationEntries } from './thread.ts';
import type { ChatEntry } from './thread.ts';
import { ChatText } from './ChatText.tsx';
import { FullOutput } from '../LiveWork.tsx';

export const CHAT_REASON_TEXT: Readonly<Record<ChatReason, string>> = {
  agent_absent: 'The agent is no longer on the floor.',
  agent_retired: 'The agent was retired.',
  halted: 'A halt stands on this agent. Chat cannot lift it.',
  unsupported_delivery: 'This agent cannot receive notes through its current delivery channel.',
  goal_absent: 'The required goal is unavailable for this message.',
  queue_full: 'The browser-note queue is full. Nothing was added.',
  paused: 'Delivery waits while this agent is paused.',
  hold: 'Delivery waits while this agent is held.',
  open_question: 'Delivery waits for the open question to close. A note does not answer it.',
  budget: 'Delivery waits for the existing budget admission checks.',
  ceiling_required: 'Set a spending limit for this engine before chatting.',
  hops: 'The original goal has no admitted turns remaining.',
  provenance: 'Delivery waits for an observed turn outcome.',
  goal_changed: 'The original goal changed. This note stays bound to its original goal.',
  context_changed: 'The conversation context changed. This message needs a new send after you review the context loss.',
  mode_changed: 'The delivery mode changed. This note stays bound to its original destination.',
  delivery_uncertain: 'Delivery before restart is unknown.',
  delivery_started: 'Delivery has started. Its text cannot be retracted.',
  note_unknown: 'No undelivered browser note matches this identity.',
  note_cancelled: 'This note was already cancelled.',
};

export function ChatPanel({ floor, row, fullOutput = false, onFullOutput }: {
  readonly floor: FloorState; readonly row: AgentRow; readonly fullOutput?: boolean;
  readonly onFullOutput?: (open: boolean) => void;
}) {
  const chat = useSyncExternalStore(subscribeToChat, getChatState, getChatState);
  const target = chat.answerTargets.get(row.id);
  const previousTarget = useRef(target);
  useLayoutEffect(() => {
    if (previousTarget.current !== target) {
      document.getElementById('chat-compose-title')?.parentElement?.querySelector<HTMLTextAreaElement>('textarea')?.focus();
      previousTarget.current = target;
    }
  }, [target]);
  useEffect(() => {
    if (chatGate(floor, row).open && floor.chat !== null) reconcileChat(floor.chat);
  }, [floor.chat, floor.phase, floor.stale, floor.epoch, floor.snapshotEpoch, row, chat.acknowledged]);
  return <ChatPanelView floor={floor} row={row} action={chat} fullOutput={fullOutput} onFullOutput={onFullOutput} />;
}

export function ChatPanelView({ floor, row, action, fullOutput = false, onFullOutput = () => {} }: {
  readonly floor: FloorState; readonly row: AgentRow; readonly action: ChatState;
  readonly fullOutput?: boolean; readonly onFullOutput?: (open: boolean) => void;
}) {
  const gate = chatGate(floor, row);
  const view = floor.chat;
  if (!gate.open || view === null) return null;
  const entries = conversationEntries(view, row.id, action.acknowledged);
  const draft = chatDraft(action, row.id);
  const problem = noteProblem(draft);
  const attempt = action.attempt?.payload.agent === row.id ? action.attempt : null;
  const blocked = action.attempt !== null || problem !== null;
  const name = typeof row.name === 'string' ? row.name : row.id;
  const question = answerGate(floor, row);
  const target = action.answerTargets.get(row.id);
  const replying = target !== undefined && question.open && target.questionLsn === question.questionLsn;
  const waitingQuestion = row.status === 'waiting' && typeof row.question === 'string' && canonicalLsn(row.question_lsn);
  return <div className="chat-panel" data-chat-agent-id={row.id}>
    <ChatThread key={row.id} entries={entries} floor={floor} row={row} action={action}
      fullOutput={fullOutput} onFullOutput={onFullOutput} />
    {row.mode === 'attached' ? <p className="chat-guidance">Attached replies are unavailable. Messages are sent as context.</p> : null}
    <section aria-labelledby="chat-compose-title" className="chat-compose">
      <h3 id="chat-compose-title" tabIndex={-1}>{replying ? 'Reply to' : 'Message'} {name}</h3>
      {replying ? <div className="chat-question" data-chat-question-lsn={target.questionLsn}>
        <blockquote>{target.question}</blockquote>
        <p>Your ordinary message draft is kept separately.</p>
        <button type="button" onClick={() => selectChatAnswer(row.id, null)}>Back to message</button>
        <AnswerForm floor={floor} row={row} leaveFocusTo="chat-compose-title" compact />
      </div> : <>
      {target === undefined ? null : <p role="status">Your reply target changed or closed. Your message draft is below.</p>}
      {waitingQuestion ? <div className="chat-question">
        <p>{name} asks:</p><blockquote>{row.question as string}</blockquote>
        {question.open ? <button type="button" disabled={action.attempt !== null}
          onClick={() => selectChatAnswer(row.id, { questionLsn: question.questionLsn, question: question.question })}>
          Reply to question</button> : <p>Answering this question from the Floor is unavailable.</p>}
        <p>A normal message does not answer this question.</p>
      </div> : null}
      <label className="chat-composer-label" htmlFor="chat-text">Your message</label>
      <textarea id="chat-text" name="chat_text" value={draft} rows={3} placeholder="Write a message…"
        aria-describedby="chat-text-limit" onChange={(event) => editChatDraft(row.id, event.currentTarget.value)}
        onKeyDown={(event) => {
          if (!queuesChatOnKey(event) || blocked || action.sending) return;
          event.preventDefault();
          void submitChat(gate, browserChatDeps);
        }} />
      {action.attempt !== null && attempt === null ? <p role="status">
        A request for another agent is unresolved. Select that agent to check its status before sending.</p> : null}
      <div className="chat-compose-actions">
        <p id="chat-text-limit">Enter to send · Shift+Enter for a newline.</p>
        <button type="button" className="chat-send" disabled={blocked}
          onClick={() => void submitChat(gate, browserChatDeps)}>Send</button>
      </div>
      {draft !== '' && problem !== null ? <p role="alert">{problem}</p> : null}
      {action.problem === null ? null : <p role="alert">{action.problem}</p>}
      </>}
      <details className="chat-guidance"><summary>Conversation details</summary>
        <p>Only a limited history is retained; individual turns may be unavailable.</p>
        {row.mode === 'attached' ? <p>Messages wait for a supported hook before they are sent as context.</p>
          : <p>Messages during active goal work join an admitted goal turn. Otherwise, each message can start one conversation turn.
            Queuing does not confirm delivery, and a reply does not confirm the goal is complete.</p>}
        <p>Messages can contain up to 4096 UTF-8 bytes. Drafts stay with each agent in this tab until reload.</p>
      </details>
    </section>
  </div>;
}

function ChatThread({ entries, floor, row, action, fullOutput, onFullOutput }: {
  readonly entries: readonly ChatEntry[]; readonly floor: FloorState; readonly row: AgentRow; readonly action: ChatState;
  readonly fullOutput: boolean; readonly onFullOutput: (open: boolean) => void;
}) {
  const scroll = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  useLayoutEffect(() => {
    if (follow.current && scroll.current !== null) scroll.current.scrollTop = scroll.current.scrollHeight;
  }, [entries, floor.output, action.attempt, action.outcome]);
  const gate = chatGate(floor, row);
  const name = typeof row.name === 'string' ? row.name : row.id;
  const attempt = action.attempt?.payload.agent === row.id ? action.attempt : null;
  const outcome = action.outcome;
  const latestTurn = row.mode === 'managed' ? entries.filter(entry => entry.kind !== 'context' && entry.turn !== null).at(-1) : undefined;
  return <section className="chat-thread" aria-labelledby="chat-thread-title">
    <h3 id="chat-thread-title">Conversation</h3>
    <div className="chat-scroll" ref={scroll} role="log" aria-label={`Conversation with ${name}`} aria-relevant="additions"
      onScroll={event => {
        const node = event.currentTarget;
        follow.current = node.scrollHeight - node.scrollTop - node.clientHeight < 48;
      }}>
      {entries.length === 0 && attempt === null ? <div className="chat-empty">
        <p>No messages in the available history</p>
        <p>Write a message below to continue the conversation.</p>
      </div> : <ol>{entries.map(entry => {
        if (entry.kind === 'context') return <li key={entry.noteLsn} className="chat-context" data-chat-context-lsn={entry.noteLsn}>
          <p>Sent as context</p>
        </li>;
        const { pending: note, turn } = entry;
        const preview = turn !== null && previewMatches(floor, row, turn);
        const status = turn !== null && note === null ? turnText(turn, preview ? floor.output.status : null) : null;
        return <li key={entry.noteLsn} className="chat-exchange" data-chat-note-lsn={entry.noteLsn}
          data-chat-pending-state={note?.state} data-chat-turn-id={turn?.turn_id} data-chat-turn-state={turn?.state}>
          <div className="chat-message chat-user">
            <p className="chat-speaker">You</p>
            <ChatText text={entry.text} />
          </div>
          {turn?.reissued === true ? <div className="chat-warning" data-chat-reissued="true">
            <p>Restarted after interruption</p>
            <details><summary>What changed</summary>
              <p>Previous delivery may have happened and earlier context may be missing.</p></details>
          </div> : null}
          {note === null ? turn === null ? <p className="chat-message-state">Message accepted; current status unavailable.</p> : null
            : <div className="chat-message-state">
              <p>{pendingText(note)}</p>
              {note.reason === null || note.reason === 'delivery_uncertain' ? null
                : <p className="chat-reason" data-chat-reason={note.reason}>{CHAT_REASON_TEXT[note.reason]}</p>}
              {note.cancellable ? <button type="button" className="chat-cancel" disabled={action.attempt !== null}
                onClick={() => void cancelChat(gate, note, browserChatDeps)}>
                {note.state === 'delivery_uncertain' ? 'Stop future reissue' : 'Cancel note'}</button>
                : <p>Cancellation is unavailable for this message.</p>}
            </div>}
          {status === null ? null : <p className="chat-message-state" role="status">{status}</p>}
          {turn?.reply == null ? null : <div className="chat-message chat-assistant">
            <p className="chat-speaker">{name}</p>
            <ChatText text={turn.reply} final />
            {turn.truncated ? <p className="chat-warning">Final reply was truncated.</p> : null}
            {turn.truncated === null ? <details className="chat-reply-details"><summary>Reply details</summary>
              <p>Final reply completeness is not recorded.</p></details> : null}
          </div>}
          {preview ? <div className="chat-message chat-assistant chat-preview" data-chat-preview="provisional">
            <p className="chat-speaker">{name}</p>
            {floor.output.reply !== null && floor.output.reply.text !== '' ? <ChatText text={floor.output.reply.text} />
              : null}
            {floor.output.truncated ? <p className="chat-warning">Earlier live output was truncated.</p> : null}
            {floor.output.reply?.gap !== null ? <div className="chat-warning" role="status"><p>Live output is incomplete.</p>
              <button type="button" onClick={() => selectOutputAgent(row.id, true)}>Refresh live output</button></div> : null}
            <details className="chat-reply-details"><summary>Reply details</summary>
              <p>Live output is provisional. It is not a final reply.</p></details>
          </div> : null}
          {entry === latestTurn
            ? <FullOutput floor={floor} row={row} id="chat-full-output" open={fullOutput} onToggle={onFullOutput} /> : null}
        </li>;
      })}</ol>}
      {attempt === null ? null : <section className="chat-receipt" aria-label="Chat request" aria-live="polite"
        data-chat-outcome={action.outcome?.kind ?? 'sending'}
        data-chat-refusal-reason={action.outcome?.kind === 'refused' ? action.outcome.reason : undefined}
        data-chat-receipt-note-lsn={action.outcome !== null && 'noteLsn' in action.outcome ? action.outcome.noteLsn : undefined}>
        {'text' in attempt.payload ? <><p className="chat-speaker">You</p><ChatText text={attempt.payload.text} /></> : null}
        <p>{outcomeText(action)}</p>
        {outcome?.kind === 'context_lost' ? <button type="button" className="chat-start-fresh" disabled={action.sending}
          ref={handFocusOnLeave} data-focus-on-leave="chat-text"
          onClick={() => void startFreshChat(attempt, outcome, browserChatDeps)}>Start fresh and resend this message</button> : null}
        {chatReleasable(action)
          ? <button type="button" className="chat-dismiss" ref={handFocusOnLeave} data-focus-on-leave="chat-text"
            onClick={releaseChat}>Continue composing</button>
          : <button type="button" className="chat-retry" disabled={action.sending || chatSettled(action)}
            ref={handFocusOnLeave} data-focus-on-leave="chat-text"
            onClick={() => void retryChat(browserChatDeps)}>Check status</button>}
      </section>}
    </div>
  </section>;
}

function outcomeText(action: ChatState): string {
  const outcome = action.outcome;
  if (action.sending) return 'Sending request…';
  if (outcome === null) return 'No request result is available.';
  switch (outcome.kind) {
    case 'queued': return 'Recorded in the journal. The note is queued; delivery is not confirmed.';
    case 'cancelled': return 'Cancelled before delivery. No text was dispatched for this note.';
    case 'reissue_cancelled': return 'Future reissue stopped. Delivery before restart is unknown; past delivery may have occurred.';
    case 'refused': return CHAT_REASON_TEXT[outcome.reason];
    case 'context_lost': return 'The agent lost its conversational context. This message was not sent. Starting fresh resends it without restoring the previous conversation.';
    case 'forbidden': return 'This session was refused. A fresh authenticated snapshot is required.';
    case 'conflict': return 'This request ID names a different request. No new effect was recorded.';
    case 'rejected': return 'The daemon refused the request format. No effect was recorded.';
    case 'pending': return 'The request is still pending. Check status repeats the same frozen request.';
    case 'unavailable': return 'The daemon could not establish durability. Check status repeats the same frozen request.';
    case 'unknown': return 'The request outcome is unknown. Check status repeats the same frozen request.';
  }
}

function pendingText(note: PendingNote): string {
  return note.state === 'queued' ? 'Queued. Delivery has not started.'
    : note.state === 'reserved' ? 'Reserved for a turn. Delivery has not started.'
      : 'Delivery before restart is unknown. Stopping future reissue cannot retract possible past delivery.';
}

function turnText(turn: ChatTurn, output: OutputStatus | null): string | null {
  switch (turn.state) {
    case 'reserved': return 'Reserved. Delivery has not started.';
    case 'running': return output === 'ended' ? 'Waiting for reply status' : 'Replying…';
    case 'ended': return turn.reply === null ? 'No final reply was reported' : null;
    case 'failed': return turn.reply === null ? 'Turn failed. No final reply was reported' : 'Turn failed.';
    case 'cancelled': return 'Cancelled before delivery.';
    case 'delivery_uncertain': return 'Delivery before restart is unknown.';
    case 'reissue_cancelled': return 'Future reissue stopped. Delivery before restart is unknown.';
  }
}

function previewMatches(floor: FloorState, row: AgentRow, turn: ChatTurn): boolean {
  return row.mode === 'managed' && turn.state === 'running' && floor.output.view === 'reply' && floor.output.reply !== null
    && ['live', 'ended', 'gap'].includes(floor.output.status)
    && floor.output.targetId === row.id && floor.output.agentId === row.id && floor.output.turnId === turn.turn_id
    && floor.output.selectionEpoch !== null;
}
