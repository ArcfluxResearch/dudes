// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore } from 'react';
import { answerGate, canonicalLsn } from '../answer/answer.ts';
import { AnswerForm } from '../answer/AnswerPanel.tsx';
import { selectOutputAgent } from '../floor/store.ts';
import type { FloorState, OutputStatus } from '../floor/store.ts';
import { handFocusOnLeave } from '../focus.ts';
import { PARTIAL_EDITS_NOTE, controlGate } from '../control/control.ts';
import type { AgentRow } from '../scene/cast.ts';
import { advanceStopAndSend, browserChatDeps, canStopAndSend, cancelChat, stopAndSend, clearQueuedChat, queuedChatNotes, chatDraft, chatFiles, chatGate, chatReleasable, chatSendProblem,
  chatSettled, chatUploading, dropChatFiles, editChatDraft, getChatState, overChatFiles, pasteChatFiles, pickChatFiles,
  queuesChatOnKey, reconcileChat, releaseChat, removeChatFile,
  retryChat, selectChatAnswer, startFreshChat, submitChat, subscribeToChat } from './chat.ts';
import { formatBytes } from './chatFiles.ts';
import type { ChatState } from './chat.ts';
import type { ChatReason, ChatTurn, PendingNote } from './model.ts';
import { conversationEntries } from './thread.ts';
import type { ChatEntry } from './thread.ts';
import { ChatText } from './ChatText.tsx';
import { Steps } from './Steps.tsx';
import { EngineSaid } from './EngineSaid.tsx';
import { FullOutput } from '../LiveWork.tsx';
import { MessageSquare, Paperclip, SendHorizontal } from 'lucide-react';
import { Button, Disclosure, Field, ICON, Notice, RING, Textarea, buttonClass, cn } from '../../ui/index.ts';
import { ASSISTANT_BUBBLE, HEADING, NOTE, SPEAKER, USER_BUBBLE } from './look.ts';
import { SHORT_THREAD } from './shortLayout.ts';

const WARNING = 'chat-warning m-0 text-small font-semibold leading-normal text-ink';
const BOX = 'rounded-lg border border-line bg-surface p-3';

export const CHAT_REASON_TEXT: Readonly<Record<ChatReason, string>> = {
  agent_absent: 'The agent is no longer on the floor.',
  agent_retired: 'The agent was retired.',
  halted: 'This agent is stopped. Press Resume in Overview if you stopped it from this page; otherwise review its status there.',
  unsupported_delivery: 'This agent cannot receive messages in this session.',
  goal_absent: 'The task this message belongs to is unavailable.',
  queue_full: 'Too many messages are waiting. Remove a queued note or wait for delivery.',
  paused: 'Delivery waits while this agent is paused.',
  hold: 'Delivery waits while this agent is held.',
  open_question: 'Delivery waits for the open question to close. A note does not answer it.',
  budget: 'Delivery waits until the spending limit allows more work.',
  ceiling_required: 'This account needs a spending limit. Review the project limits in Settings before sending.',
  hops: 'This task has used all its work attempts. Open Overview to review what happens next.',
  provenance: 'Delivery waits while Dudes checks how the previous work ended.',
  goal_changed: 'The task changed. This note still belongs to the earlier task.',
  context_changed: 'The conversation context changed. This message needs a new send after you review the context loss.',
  mode_changed: 'The session changed. This note still belongs to the earlier session.',
  delivery_uncertain: 'Delivery before restart is unknown.',
  delivery_started: 'Delivery has started. Its text cannot be retracted.',
  note_unknown: 'This note is no longer waiting for delivery. Check the conversation for its status.',
  note_cancelled: 'This note was already cancelled.',
  stale_control: 'This agent changed after the stop, so the note was not sent. It is still in the box.',
  turn_open: 'The stopped turn has not ended yet, so the note was not sent. It is still in the box.',
  action_off: 'Stopping and sending is switched off, so the note was not sent. It is still in the box.',
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
    void advanceStopAndSend(browserChatDeps);
  }, [floor.chat, floor.phase, floor.stale, floor.epoch, floor.snapshotEpoch, row, chat.acknowledged, chat.stopSend]);
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
  const files = chatFiles(action, row.id);
  const problem = chatSendProblem(action, row.id);
  const attempt = action.attempt?.payload.agent === row.id ? action.attempt : null;
  const blocked = action.attempt !== null || action.clearing != null || problem !== null || chatUploading(action, row.id);
  const stopControl = controlGate(floor, row);
  const accepts = row.mode !== 'attached';
  const name = typeof row.name === 'string' ? row.name : 'this agent';
  const question = answerGate(floor, row);
  const target = action.answerTargets.get(row.id);
  const replying = target !== undefined && question.open && target.questionLsn === question.questionLsn;
  const waitingQuestion = row.status === 'waiting' && typeof row.question === 'string' && canonicalLsn(row.question_lsn);
  return <div className="chat-panel flex h-full min-h-0 flex-col gap-3 overflow-y-auto font-sans text-base text-ink" data-chat-agent-id={row.id}>
    <ChatThread key={row.id} entries={entries} floor={floor} row={row} action={action}
      fullOutput={fullOutput} onFullOutput={onFullOutput} />
    {row.mode === 'attached' ? <p className={cn('chat-guidance', NOTE)}>Attached replies are unavailable. Messages are sent as context.</p> : null}
    <section aria-labelledby="chat-compose-title" className="chat-compose sticky bottom-0 z-10 flex shrink-0 flex-col gap-2 border-t border-line bg-surface pt-3"
      onDragOver={overChatFiles} onDrop={(event) => dropChatFiles(event, gate)}>
      <h3 id="chat-compose-title" tabIndex={-1} className={cn(HEADING, RING, 'rounded-sm')}>{replying ? 'Reply to' : 'Message'} {name}</h3>
      {replying ? <div className={cn('chat-question flex flex-col gap-2 bg-surface-2', BOX)} data-chat-question-lsn={target.questionLsn}>
        <blockquote className="m-0 border-s-[3px] border-warn ps-3 whitespace-pre-wrap [overflow-wrap:anywhere]">{target.question}</blockquote>
        <p className={NOTE}>Your ordinary message draft is kept separately.</p>
        <Button size="sm" className="self-start" onClick={() => selectChatAnswer(row.id, null)}>Back to message</Button>
        <AnswerForm floor={floor} row={row} leaveFocusTo="chat-compose-title" compact />
      </div> : <>
      {target === undefined ? null : <p className={NOTE} role="status">Your reply target changed or closed. Your message draft is below.</p>}
      {waitingQuestion ? <div className={cn('chat-question flex flex-col gap-2 bg-surface-2', BOX)}>
        <p className="m-0 font-medium">{name} asks:</p>
        <blockquote className="m-0 border-s-[3px] border-warn ps-3 whitespace-pre-wrap [overflow-wrap:anywhere]">{row.question as string}</blockquote>
        {question.open ? <Button size="sm" className="self-start" disabled={action.attempt !== null}
          onClick={() => selectChatAnswer(row.id, { questionLsn: question.questionLsn, question: question.question })}>
          Reply to question</Button> : <p className={NOTE}>Answering this question from the Floor is unavailable.</p>}
        <p className={NOTE}>A normal message does not answer this question.</p>
      </div> : null}
      <Field label="Your message" id="chat-text">
        <Textarea name="chat_text" value={draft} rows={3} placeholder="Write a message…"
          className="max-h-72 min-h-20 text-body"
          aria-describedby="chat-text-limit" onChange={(event) => editChatDraft(row.id, event.currentTarget.value)}
          onPaste={(event) => pasteChatFiles(event, gate)}
          onKeyDown={(event) => {
            if (!queuesChatOnKey(event) || blocked || action.sending) return;
            event.preventDefault();
            void submitChat(gate, browserChatDeps);
          }} />
      </Field>
      {action.attempt !== null && attempt === null ? <p className={NOTE} role="status">
        A request for another agent is unresolved. Select that agent to check its status before sending.</p> : null}
      {files.length === 0 ? null : <ul className="chat-files m-0 flex list-none flex-col gap-1.5 p-0" aria-label="Attached files">
        {files.map(file => <li key={file.id} data-chat-file-status={file.status}
          className={cn('chat-file flex flex-wrap items-center gap-x-3 gap-y-1 rounded-md border bg-surface px-2 py-1.5 text-small',
            file.status === 'failed' ? 'border-danger' : 'border-line')}>
          <span className="chat-file-name min-w-0 font-medium [overflow-wrap:anywhere]">{file.name}</span>
          <span className="chat-file-size text-muted">{formatBytes(file.size)}</span>
          <span className={cn('chat-file-state min-w-32 flex-1 [overflow-wrap:anywhere]', file.status === 'failed' ? 'font-medium text-danger' : 'text-muted')}
            role={file.status === 'failed' ? 'alert' : 'status'}>
            {file.status === 'uploading' ? 'Uploading…' : file.status === 'failed' ? file.error : 'Ready'}</span>
          <Button size="sm" variant="ghost" className="chat-file-remove" aria-label={`Remove ${file.name}`}
            onClick={() => removeChatFile(row.id, file.id)}>Remove</Button>
        </li>)}
      </ul>}
      <div className="chat-compose-actions flex flex-wrap items-center justify-between gap-x-3 gap-y-2">
        <p id="chat-text-limit" className={cn(NOTE, 'min-w-48 flex-1 text-caption')}>{accepts ? 'Enter to send · Shift+Enter for a newline · Paste or drop files to attach.'
          : 'Enter to send · Shift+Enter for a newline.'}</p>
        <div className="chat-compose-buttons ms-auto flex items-center gap-2">
          {accepts ? <label title="Attach files"
            className={buttonClass('secondary', 'md', true, 'chat-attach relative has-[:focus-visible]:outline-2 has-[:focus-visible]:outline-solid has-[:focus-visible]:outline-focus has-[:focus-visible]:outline-offset-2')}>
            <input type="file" multiple className="chat-file-input absolute inset-0 size-full cursor-pointer opacity-0" aria-label="Attach files"
              onChange={(event) => pickChatFiles(event, gate)} />
            <Paperclip {...ICON} aria-hidden="true" />
          </label> : null}
          {stopControl.open && stopControl.stop ? <Button className="chat-stop-send"
            disabled={blocked || !canStopAndSend(action, gate, stopControl)} aria-describedby="chat-stop-send-note"
            title="Stop this turn, then send your message once it has ended"
            onClick={() => void stopAndSend(gate, stopControl, browserChatDeps)}>Stop and send</Button> : null}
          <Button variant="primary" className="chat-send" icon={<SendHorizontal {...ICON} aria-hidden="true" />}
            disabled={blocked || action.stopSend !== null}
            onClick={() => void submitChat(gate, browserChatDeps)}>Send</Button>
        </div>
      </div>
      {stopControl.open && stopControl.stop ? <p className={NOTE} id="chat-stop-send-note">
        Stop and send ends the running turn, then sends your message as a note. {PARTIAL_EDITS_NOTE}</p> : null}
      {action.stopSend?.agent === row.id ? <p className={NOTE} role="status" data-chat-stop-send={action.stopSend.stopLsn === null ? 'stopping' : 'waiting'}>
        {action.stopSend.stopLsn === null ? 'Stopping this turn…'
          : 'Stopped. Sending your note once the turn has ended…'}</p> : null}
      {accepts ? null : <p className={cn('chat-guidance', NOTE)}>Files work with agents Dudes launches.</p>}
      {(draft !== '' || files.length > 0) && problem !== null ? <Notice tone="danger" role="alert">{problem}</Notice> : null}
      {action.problem === null ? null : <Notice tone="danger" role="alert">{action.problem}</Notice>}
      </>}
    </section>
  </div>;
}

function ChatThread({ entries, floor, row, action, fullOutput, onFullOutput }: {
  readonly entries: readonly ChatEntry[]; readonly floor: FloorState; readonly row: AgentRow; readonly action: ChatState;
  readonly fullOutput: boolean; readonly onFullOutput: (open: boolean) => void;
}) {
  const scroll = useRef<HTMLDivElement>(null);
  const follow = useRef(true);
  const size = useRef({ width: 0, height: 0 });
  useLayoutEffect(() => {
    const node = scroll.current;
    if (!node) return;
    size.current = { width: node.clientWidth, height: node.clientHeight };
    if (typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => {
      size.current = { width: node.clientWidth, height: node.clientHeight };
      if (follow.current) node.scrollTop = node.scrollHeight;
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  useLayoutEffect(() => {
    const drawer = scroll.current?.closest<HTMLElement>('.agent-drawer');
    const viewport = window.visualViewport;
    if (!drawer || !viewport) return;
    const fit = () => {
      drawer.style.setProperty('--chat-viewport-height', `${viewport.height}px`);
      drawer.style.setProperty('--chat-viewport-top', `${viewport.offsetTop}px`);
      drawer.setAttribute('data-chat-short', String(viewport.height < 600));
    };
    fit();
    viewport.addEventListener('resize', fit);
    viewport.addEventListener('scroll', fit);
    return () => {
      viewport.removeEventListener('resize', fit);
      viewport.removeEventListener('scroll', fit);
      drawer.style.removeProperty('--chat-viewport-height');
      drawer.style.removeProperty('--chat-viewport-top');
      drawer.removeAttribute('data-chat-short');
    };
  }, []);
  useLayoutEffect(() => {
    if (follow.current && scroll.current !== null) scroll.current.scrollTop = scroll.current.scrollHeight;
  }, [entries, floor.output, action.attempt, action.outcome]);
  const gate = chatGate(floor, row);
  const name = typeof row.name === 'string' ? row.name : 'this agent';
  const attempt = action.attempt?.payload.agent === row.id ? action.attempt : null;
  const outcome = action.outcome;
  const latestTurn = row.mode === 'managed' ? entries.filter(entry => entry.kind !== 'context' && entry.turn !== null).at(-1) : undefined;
  return <section className={cn('chat-thread flex min-h-72 min-w-0 flex-1 flex-col gap-2', SHORT_THREAD)} aria-labelledby="chat-thread-title">
    <div className="chat-thread-heading flex flex-wrap items-center justify-between gap-x-3 gap-y-1"><h3 id="chat-thread-title" className={HEADING}>Conversation</h3>
      <ClearQueuedNotes floor={floor} row={row} action={action} />
    </div>
    <div className="chat-scroll min-h-0 flex-1 overflow-y-auto rounded-lg border border-line bg-bg p-3 [overflow-anchor:none]"
      ref={scroll} role="log" aria-label={`Conversation with ${name}`} aria-relevant="additions"
      onScroll={event => {
        const node = event.currentTarget;
        if (node.clientWidth !== size.current.width || node.clientHeight !== size.current.height) return;
        follow.current = node.scrollHeight - node.scrollTop - node.clientHeight < 48;
      }}>
      {entries.length === 0 && attempt === null ? <div className="chat-empty grid min-h-full content-center justify-items-center gap-1 text-center">
        <MessageSquare {...ICON} size={24} aria-hidden="true" className="mb-1 text-subtle" />
        <p className="m-0 font-medium">No messages in the available history</p>
        <p className={NOTE}>Write a message below to continue the conversation.</p>
      </div> : <ol className="m-0 grid min-h-full list-none content-end gap-4 p-0">{entries.map(entry => {
        if (entry.kind === 'context') return <li key={entry.noteLsn} className="chat-context rounded-md border border-dashed border-line-strong px-3 py-2 text-small text-muted" data-chat-context-lsn={entry.noteLsn}>
          <p className="m-0">Sent as context</p>
        </li>;
        const { pending: note, turn } = entry;
        const preview = turn !== null && previewMatches(floor, row, turn);
        const status = turn !== null && note === null ? turnText(turn, preview ? floor.output.status : null) : null;
        const emphasis = turn?.state === 'failed' || note?.state === 'delivery_uncertain' ? 'font-semibold' : '';
        return <li key={entry.noteLsn} className="chat-exchange grid min-w-0 gap-1.5" data-chat-note-lsn={entry.noteLsn}
          data-chat-pending-state={note?.state} data-chat-turn-id={turn?.turn_id} data-chat-turn-state={turn?.state}>
          <div className={cn('chat-message chat-user', USER_BUBBLE)}>
            <p className={cn('chat-speaker', SPEAKER)}>You</p>
            <ChatText text={entry.text} />
          </div>
          {turn?.reissued === true ? <Notice tone="warn" data-chat-reissued="true">
            <p className={WARNING}>Restarted after interruption</p>
            <Disclosure className="font-normal" summaryClassName="py-0 text-small font-normal" summary="What changed">
              <p className={NOTE}>Previous delivery may have happened and earlier context may be missing.</p></Disclosure>
          </Notice> : null}
          {note === null ? turn === null ? <p className={cn('chat-message-state mx-2', NOTE)}>Message accepted; current status unavailable.</p> : null
            : <div className={cn('chat-message-state mx-2 flex flex-col items-start gap-1', NOTE, emphasis)}>
              <p className="m-0">{pendingText(note)}</p>
              {note.reason === null || note.reason === 'delivery_uncertain' ? null
                : <p className="chat-reason m-0 font-semibold" data-chat-reason={note.reason}>{CHAT_REASON_TEXT[note.reason]}</p>}
              {note.cancellable ? <Button size="sm" className="chat-cancel" disabled={action.attempt !== null || action.clearing != null}
                onClick={() => void cancelChat(gate, note, browserChatDeps)}>
                {note.state === 'delivery_uncertain' ? 'Stop future reissue' : 'Cancel note'}</Button>
                : <p className="m-0">Cancellation is unavailable for this message.</p>}
            </div>}
          {status === null ? null : <p className={cn('chat-message-state mx-2', NOTE, emphasis)} role="status">{status}</p>}
          {turn !== null && stepsMatch(floor, row, turn)
            ? <Steps steps={floor.output.steps} truncated={floor.output.stepsTruncated} stopped={floor.output.stepsStopped} results={floor.output.stepResults} /> : null}
          {turn?.failure == null ? null : <EngineSaid failure={turn.failure} />}
          {turn?.reply == null || turn.reply === turn.failure ? null : <div className={cn('chat-message chat-assistant', ASSISTANT_BUBBLE)}>
            <p className={cn('chat-speaker', SPEAKER)}>{name}</p>
            <ChatText text={turn.reply} final mode="agent" />
            {turn.truncated ? <p className={cn(WARNING, 'mt-2')}>Final reply was truncated.</p> : null}
            {turn.truncated === null ? <Disclosure className="chat-reply-details mt-1" summaryClassName="py-0 text-small font-normal text-muted" summary="Reply details">
              <p className={NOTE}>Final reply completeness is not recorded.</p></Disclosure> : null}
          </div>}
          {preview ? <div className={cn('chat-message chat-assistant chat-preview border-s-2 border-s-accent', ASSISTANT_BUBBLE)} data-chat-preview="provisional">
            <p className={cn('chat-speaker', SPEAKER)}>{name}</p>
            {floor.output.reply !== null && floor.output.reply.text !== '' ? <ChatText text={floor.output.reply.text} mode="agent" />
              : null}
            {floor.output.truncated ? <p className={cn(WARNING, 'mt-2')}>Earlier live output was truncated.</p> : null}
            {floor.output.reply?.gap !== null ? <div className={cn(WARNING, 'mt-2 flex flex-wrap items-center gap-2')} role="status"><p className="m-0">Live output is incomplete.</p>
              <Button size="sm" onClick={() => selectOutputAgent(row.id, true)}>Refresh live output</Button></div> : null}
            <Disclosure className="chat-reply-details mt-1" summaryClassName="py-0 text-small font-normal text-muted" summary="Reply details">
              <p className={NOTE}>Live output is provisional. It is not a final reply.</p></Disclosure>
          </div> : null}
          {entry === latestTurn
            ? <FullOutput floor={floor} row={row} id="chat-full-output" open={fullOutput} onToggle={onFullOutput} /> : null}
        </li>;
      })}</ol>}
      {attempt === null ? null : <section className={cn('chat-receipt mt-3 flex flex-col items-start gap-2', BOX)} aria-label="Chat request" aria-live="polite"
        data-chat-outcome={action.outcome?.kind ?? 'sending'}
        data-chat-refusal-reason={action.outcome?.kind === 'refused' ? action.outcome.reason : undefined}
        data-chat-receipt-note-lsn={action.outcome !== null && 'noteLsn' in action.outcome ? action.outcome.noteLsn : undefined}>
        {'text' in attempt.payload ? <><p className={cn('chat-speaker', SPEAKER)}>You</p><ChatText text={attempt.payload.text} /></> : null}
        <p className="m-0">{outcomeText(action)}</p>
        <Disclosure summaryClassName="py-0 text-small font-normal text-muted" summary="Details">
          <div className={cn('flex flex-col gap-0.5', NOTE)}>
            <p className="m-0">Request: <code className="font-mono text-[0.88em]">{attempt.payload.request_id}</code></p>
            {'note_lsn' in attempt.payload ? <p className="m-0">Note: <code className="font-mono text-[0.88em]">{attempt.payload.note_lsn}</code></p> : null}
            <p className="m-0">Agent: <code className="font-mono text-[0.88em]">{attempt.payload.agent}</code></p>
            <p className="m-0">Status: <code className="font-mono text-[0.88em]">{outcome?.kind ?? 'sending'}</code></p>
          </div>
        </Disclosure>
        {outcome?.kind === 'context_lost' && 'after_stop' in attempt.payload
          ? <p className="m-0" role="status">The stopped turn left no conversation to continue. Press Resume to let the agent work
            again, then send a new message. Your note is still in the box.</p> : null}
        {outcome?.kind === 'context_lost' && !('after_stop' in attempt.payload) ? <Button className="chat-start-fresh" disabled={action.sending}
          ref={handFocusOnLeave} data-focus-on-leave="chat-text"
          onClick={() => void startFreshChat(attempt, outcome, browserChatDeps)}>Start fresh and resend this message</Button> : null}
        {chatReleasable(action)
          ? <Button variant="primary" className="chat-dismiss" disabled={action.clearing != null} ref={handFocusOnLeave} data-focus-on-leave="chat-text"
            onClick={releaseChat}>Continue composing</Button>
          : <Button className="chat-retry" disabled={action.sending || chatSettled(action)}
            ref={handFocusOnLeave} data-focus-on-leave="chat-text"
            onClick={() => void retryChat(browserChatDeps)}>Check status</Button>}
      </section>}
      <Disclosure className="chat-guidance mt-3" summaryClassName="py-0 text-small font-normal text-muted" summary="Conversation details">
        <div className={cn('flex flex-col gap-1', NOTE)}>
          <p className="m-0">Only recent history is kept, so some earlier work may be missing.</p>
          {row.mode === 'attached' ? <p className="m-0">Messages wait for a supported hook before they are sent as context.</p>
            : <p className="m-0">Messages during active goal work join an admitted goal turn. Otherwise, each message can start one conversation turn.
              Queuing does not confirm delivery, and a reply does not confirm the goal is complete.</p>}
          <p className="m-0">Messages can contain up to 4096 UTF-8 bytes. Drafts stay with each agent in this tab until reload.</p>
        </div>
      </Disclosure>
    </div>
  </section>;
}

function ClearQueuedNotes({ floor, row, action }: {
  readonly floor: FloorState; readonly row: AgentRow; readonly action: ChatState;
}) {
  const [asking, setAsking] = useState(false);
  const queued = queuedChatNotes(floor, row.id);
  const blocked = action.attempt !== null || action.clearing != null;
  if (queued.length === 0 && !asking && action.clearing?.agent !== row.id) return null;
  return <div className="chat-clear flex flex-wrap items-center gap-2">
    <Button size="sm" variant="ghost" className="chat-clear-all" ref={handFocusOnLeave} data-focus-on-leave="chat-text" disabled={blocked || queued.length === 0}
      onClick={() => setAsking(true)}>Clear all</Button>
    {action.clearing?.agent === row.id ? <p className={NOTE} role="status">Removing queued notes…</p> : null}
    {asking ? <div className={cn('chat-clear-question flex basis-full flex-col items-start gap-2 bg-surface-2', BOX)} role="group" aria-label="Clear queued notes">
      <p className="m-0">Remove all queued notes for {typeof row.name === 'string' ? row.name : 'this agent'}?
        Notes already sent or with unknown delivery stay.</p>
      <div className="flex flex-wrap gap-2">
        <Button variant="danger" size="sm" className="chat-clear-confirm" ref={handFocusOnLeave} data-focus-on-leave="chat-text" disabled={blocked} onClick={() => {
          setAsking(false);
          void clearQueuedChat(chatGate(floor, row), browserChatDeps);
        }}>Remove queued notes</Button>
        <Button size="sm" className="chat-clear-keep" onClick={() => setAsking(false)}>Keep notes</Button>
      </div>
    </div> : null}
  </div>;
}

function outcomeText(action: ChatState): string {
  const outcome = action.outcome;
  if (action.sending) return 'Sending request…';
  if (outcome === null) return 'No request result is available.';
  switch (outcome.kind) {
    case 'queued': return 'The note is saved and queued. Delivery is not confirmed.';
    case 'cancelled': return 'Cancelled before delivery. No text was dispatched for this note.';
    case 'reissue_cancelled': return 'Future reissue stopped. Delivery before restart is unknown; past delivery may have occurred.';
    case 'refused': return CHAT_REASON_TEXT[outcome.reason];
    case 'context_lost': return 'The agent lost its conversational context. This message was not sent. Starting fresh resends it without restoring the previous conversation.';
    case 'forbidden': return 'This page could not confirm your access. Wait for the Floor to reconnect, then check status.';
    case 'conflict': return 'This request conflicts with an earlier action. Nothing new was saved.';
    case 'rejected': return 'Dudes could not use this request. Nothing was saved.';
    case 'pending': return 'This request is still pending. Choose Check status to see whether it finished.';
    case 'unavailable': return 'Dudes could not confirm this request was saved. Choose Check status before continuing.';
    case 'unknown': return 'The result is unknown. Choose Check status before continuing.';
  }
}

function pendingText(note: PendingNote): string {
  return note.state === 'queued' ? 'Queued. Delivery has not started.'
    : note.state === 'reserved' ? 'Next to be sent. Delivery has not started.'
      : 'Delivery before restart is unknown. Stopping future reissue cannot retract possible past delivery.';
}

function turnText(turn: ChatTurn, output: OutputStatus | null): string | null {
  switch (turn.state) {
    case 'reserved': return 'Next to be sent. Delivery has not started.';
    case 'running': return output === 'ended' ? 'Waiting for reply status' : 'Replying…';
    case 'ended': return turn.reply === null ? 'No final reply was reported' : null;
    case 'failed': return turn.reply === null && turn.failure === null ? 'Turn failed. No final reply was reported' : 'Turn failed.';
    case 'cancelled': return 'Cancelled before delivery. The agent did not receive this note.';
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

function stepsMatch(floor: FloorState, row: AgentRow, turn: ChatTurn): boolean {
  return row.mode === 'managed' && floor.output.view === 'reply' && floor.output.steps.length > 0
    && floor.output.targetId === row.id && floor.output.agentId === row.id && floor.output.turnId === turn.turn_id
    && floor.output.selectionEpoch !== null;
}
