// SPDX-License-Identifier: MIT OR Apache-2.0

import { useLayoutEffect, useMemo, useRef, useState } from 'react';
import type { CSSProperties, ReactNode } from 'react';

import { accentOf, accessoryOf, lookOf } from './appearance.ts';
import { Portrait } from './Portrait.tsx';
import { baselineOf, completionCheckOf } from './goal/completionCheck.ts';
import type { Baseline } from './goal/completionCheck.ts';
import { detailTabSelection, detailTabTarget, revealedTabScroll } from './detailTabs.ts';
import type { DetailTab } from './detailTabs.ts';
import { useLiveActivityExpiry } from './useLiveActivityExpiry.ts';
import { HIRE_ENGINES } from './onboarding/catalog.ts';
import { NameEditor } from './rename/NameEditor.tsx';
import { LookEditor } from './look/LookEditor.tsx';
import { GoalResult } from './goal/GoalResult.tsx';
import { goalResultOf } from './goal/result.ts';
import { beginRename } from './rename/rename.ts';
import { automaticHoldOf, automaticHoldText } from './automaticHold.ts';
import { AGENT_DETAIL_HEADING_ID, handFocusOnLeave } from './focus.ts';
import type { AgentRow } from './scene/cast.ts';
import { fieldsOf } from './scene/label.ts';
import { workspaceLabel } from './workspace.ts';
import { decodeWorktree, sharesCheckout, worktreeText } from './worktree.ts';

function providerLabel(provider: string): string {
  return (HIRE_ENGINES as Readonly<Record<string, string>>)[provider] ?? provider;
}

function value(value: unknown): string {
  return typeof value === 'string' && value !== '' ? value : '—';
}

const BASELINE_LINES: Readonly<Record<Baseline | 'absent', string>> = Object.freeze({
  failed: 'It failed before the agent’s first turn, so a pass shows a change.',
  may_already_pass: 'The operator marked this check as one that may already pass. Done shows only that it passes '
    + 'after the agent’s work.',
  absent: 'It did not run before the agent’s first turn.',
});

export function AgentDetail({
  row, selection = null, streamState = null, control = null, retire = null, terminal = null, enable = null, answer = null, goal: goalForm = null,
  output = null, messages = null, chat = null, terminalDetails = null, git = null, memory = null, tab = 'overview', onTabChange = () => {}, live = false,
}: {
  readonly row: AgentRow;
  readonly selection?: ReactNode;
  /** The output stream's state, drawn as the last part of the header's status line. */
  readonly streamState?: string | null;
  readonly control?: ReactNode;
  readonly retire?: ReactNode;
  readonly terminal?: ReactNode;
  /** How to turn on the controls the daemon reports off. */
  readonly enable?: ReactNode;
  readonly answer?: ReactNode;
  readonly goal?: ReactNode;
  readonly output?: ReactNode;
  readonly messages?: ReactNode;
  readonly chat?: ReactNode;
  readonly terminalDetails?: ReactNode;
  readonly git?: ReactNode;
  readonly memory?: ReactNode;
  readonly tab?: DetailTab;
  readonly onTabChange?: (tab: DetailTab) => void;
  readonly live?: boolean;
}) {
  const detail = useRef<HTMLElement>(null);
  const tabRow = useRef<HTMLDivElement>(null);
  const [focusedSection, setFocusedSection] = useState<DetailTab | null>(null);
  const rows = useMemo(() => [row], [row]);
  useLiveActivityExpiry(rows, live);
  const fields = fieldsOf(row, { live, now: performance.now(), wallNow: Date.now() });
  const automaticHold = automaticHoldOf(row);
  const worktree = decodeWorktree(row.worktree);
  const pursuing = typeof row.goal === 'object' && row.goal !== null;
  const goal = pursuing ? row.goal as Record<string, unknown> : {};
  const completionCheck = completionCheckOf(goal);
  const baseline = baselineOf(goal);
  const panels: readonly { readonly id: DetailTab; readonly label: string; readonly content: ReactNode }[] = [
    { id: 'overview', label: 'Overview', content: <>
      {goalForm}
      <LookEditor row={row} />
      <dl>
        <div><dt>AgentId</dt><dd className="data">{row.id}</dd></div>
        <div><dt>Provider</dt><dd>{value(row.provider)}</dd></div>
        <div><dt>Workspace</dt><dd>{workspaceLabel(row)}</dd></div>
        {worktree === null ? null : <div><dt>Worktree</dt><dd>{worktreeText(worktree)}</dd></div>}
        {!sharesCheckout(row) ? null : <div><dt>Shared directory</dt><dd>Another live agent works in this agent's directory, so their edits and checkouts can collide.</dd></div>}
        <div><dt>Role</dt><dd>{value(row.role)}</dd></div>
        <div><dt>Status</dt><dd>{value(fields.status)}</dd></div>
        <div><dt>Mode</dt><dd>{value(row.mode)}</dd></div>
        <div><dt>Paused</dt><dd>{row.paused === true ? 'yes' : 'no'}</dd></div>
        <div><dt>Halted</dt><dd>{row.halted === true ? 'yes' : 'no'}</dd></div>
        <div><dt>Breaker</dt><dd>{value(row.breaker)}</dd></div>
        <div><dt>Goal</dt><dd>{value(goal.text)}</dd></div>
        <div><dt>Goal status</dt><dd>{goal.status === 'achieved' && baseline === 'may_already_pass'
          ? 'achieved (the check may have passed before work)' : value(goal.status)}</dd></div>
        <div><dt>Completion check</dt><dd>{completionCheck === null ? 'No name was recorded for this check.' : <>
          <span>{completionCheck.label}</span>
          <span className="completion-check-identity">{completionCheck.id}</span>
          <span className="completion-check-identity">Revision {completionCheck.revision}</span>
          <p>This is the configured check summary. The command is not shown here.</p>
        </>}{pursuing ? <p data-check-baseline={baseline ?? 'absent'}>{BASELINE_LINES[baseline ?? 'absent']}</p> : null}</dd></div>
        <div><dt>Turns</dt><dd>{typeof row.turns === 'number' ? row.turns : 0}</dd></div>
        <div><dt>Cost today</dt><dd>{fields.cost}</dd></div>
      </dl>
      <GoalResult result={goalResultOf(row.goal)} />
    </> },
    { id: 'output', label: 'Output', content: output },
    { id: 'chat', label: 'Chat', content: chat },
    { id: 'terminal', label: 'Terminal', content: terminalDetails },
    { id: 'git', label: 'Git', content: git },
    { id: 'memory', label: 'Memory', content: memory },
    { id: 'messages', label: 'Messages', content: messages },
  ];
  const available = panels.filter((panel) => panel.content !== null);
  const tabs = available.map((panel) => panel.id);
  const selected = detailTabSelection(tabs, tab, focusedSection);
  useLayoutEffect(() => {
    const root = detail.current;
    const section = root?.ownerDocument.activeElement?.closest<HTMLElement>('[data-detail-section]');
    const focused = available.find((panel) => panel.id === section?.dataset.detailSection);
    setFocusedSection(root?.contains(section ?? null) ? focused?.id ?? null : null);
  });
  useLayoutEffect(() => {
    const strip = tabRow.current;
    const tab = strip?.querySelector<HTMLElement>('[aria-selected="true"]');
    if (!strip || !tab) return;
    const left = tab.getBoundingClientRect().left - strip.getBoundingClientRect().left + strip.scrollLeft;
    strip.scrollLeft = revealedTabScroll(strip.scrollLeft, left, tab.offsetWidth, strip.clientWidth);
  }, [selected, tabs.join()]);
  useLayoutEffect(() => {
    const strip = tabRow.current;
    if (!strip) return;
    const wheel = (event: WheelEvent) => {
      const limit = strip.scrollWidth - strip.clientWidth;
      if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey
        || event.deltaX !== 0 || event.deltaY === 0 || limit <= 0) return;
      const unit = event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? strip.clientWidth : 1;
      const next = Math.max(0, Math.min(limit, strip.scrollLeft + event.deltaY * unit));
      if (next === strip.scrollLeft) return;
      strip.scrollLeft = next;
      event.preventDefault();
    };
    strip.addEventListener('wheel', wheel, { passive: false });
    return () => strip.removeEventListener('wheel', wheel);
  }, []);

  return (
    <aside ref={detail} className="agent-detail" aria-label="Selected agent details" data-selected-agent-id={row.id}
      style={{ '--agent': accentOf(row) } as CSSProperties}>
      <header className="agent-detail-head">
        <Portrait className="agent-portrait agent-detail-portrait" look={lookOf(row)} accessory={accessoryOf(row.role)}
          name={row.name} />
        <div className="agent-detail-identity">
          <p className="agent-detail-kicker">Selected agent</p>
          <div className="agent-detail-name">
            <h2 id={AGENT_DETAIL_HEADING_ID} tabIndex={-1} onDoubleClick={() => beginRename(`detail:${row.id}`, row)}>{value(row.name)}</h2>
            <NameEditor row={row} surface="detail" showName={false} />
          </div>
          <p className="agent-detail-status">
            {value(row.role)}
            {typeof row.provider === 'string' && row.provider !== '' ? ` · ${providerLabel(row.provider)}` : ''}
            {' · '}<span data-status={fields.status}>{value(fields.status)}</span>
            {fields.liveActivity === null ? null : <>{' · '}<span data-live-activity={fields.liveActivity}>{fields.liveActivity}</span></>}
            {streamState === null ? null : <>{' · '}<span className="stream-state" role="status">{streamState}</span></>}
          </p>
        </div>
        {selection}
        {automaticHold === null ? null : <p className="agent-detail-hold" role="status">
          {automaticHoldText(automaticHold)}
        </p>}
        <div className="agent-detail-actions">{control}{retire}{terminal}{enable}</div>
        {fields.question === '' ? null : (
          <section className="agent-detail-question" aria-label="Waiting question" tabIndex={-1}>
            <h3>Waiting question</h3>
            <p>{fields.question}</p>
            {answer}
          </section>
        )}
      </header>
      <div ref={tabRow} className="agent-detail-tabs" role="tablist" aria-label="Agent detail sections"
        aria-describedby={tabs.length > 1 ? 'agent-detail-tab-hint' : undefined}>
        {available.map(({ id, label }) => <button key={id} type="button" role="tab" className="agent-detail-tab"
          id={`agent-detail-tab-${id}`} data-detail-section={id} aria-controls={`agent-detail-panel-${id}`}
          aria-selected={selected === id} tabIndex={selected === id ? 0 : -1}
          ref={handFocusOnLeave} data-focus-on-leave="agent-detail-tab-overview"
          onFocus={() => setFocusedSection(id)} onBlur={() => setFocusedSection(null)}
          onClick={() => onTabChange(id)} onKeyDown={(event) => {
            if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
            const target = detailTabTarget(tabs, id, event.key);
            if (target === null) return;
            event.preventDefault();
            onTabChange(target);
            event.currentTarget.ownerDocument.getElementById(`agent-detail-tab-${target}`)?.focus();
          }}>{label}</button>)}
      </div>
      {tabs.length > 1 ? <p id="agent-detail-tab-hint" className="agent-detail-tab-hint">Scroll for more sections · Use arrow keys</p> : null}
      {available.map(({ id, content }) => <section key={id} id={`agent-detail-panel-${id}`}
        role="tabpanel" data-detail-section={id} aria-labelledby={`agent-detail-tab-${id}`} tabIndex={0} hidden={selected !== id}
        ref={handFocusOnLeave} data-focus-on-leave="agent-detail-tab-overview"
        onFocus={() => setFocusedSection(id)} onBlur={(event) => {
          if (!event.currentTarget.contains(event.relatedTarget)) setFocusedSection(null);
        }}>
        {content}
      </section>)}
    </aside>
  );
}
