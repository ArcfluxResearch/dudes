// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ReactNode } from 'react';
import type { PlanItem, StepArg, StepItem } from '../floor/steps.ts';

const SUMMARY_CHARS = 100;
const PLAN_MARKS = { pending: '☐', in_progress: '▸', completed: '☑' } as const;
const PLAN_WORDS = { pending: 'to do', in_progress: 'in progress', completed: 'done' } as const;
const MAX_DEPTH = 8;
const OPEN_UP_TO = 8;
const KEY_WORDS: Readonly<Record<string, string>> = {
  command: 'Command', file_path: 'File', path: 'Folder', notebook_path: 'Notebook', pattern: 'Search for', query: 'Search for',
  url: 'Address', description: 'Task', prompt: 'Instructions', subagent_type: 'Helper type',
};

function firstLine(value: string): string {
  const line = value.split('\n', 1)[0] ?? '';
  return line.length > SUMMARY_CHARS ? `${line.slice(0, SUMMARY_CHARS)}…` : line;
}

function arg(step: StepItem, ...keys: readonly string[]): string | null {
  for (const key of keys) {
    const value = step.args.find((item) => item.key === key)?.value;
    if (value !== undefined && value !== '') return firstLine(value);
  }
  return null;
}

function counts(plan: readonly PlanItem[]): string {
  return `${plan.filter((item) => item.status === 'completed').length} of ${plan.length} done`;
}

/** The step in one line of plain words: what it is doing, or did. */
export function stepSummary(step: StepItem): string {
  const running = step.status === 'running';
  const verb = (doing: string, did: string) => (running ? doing : did);
  const named = (head: string, subject: string | null) => (subject === null ? head : `${head} ${subject}`);
  switch (step.kind) {
    case 'command': return named(verb('Running', 'Ran'), arg(step, 'command') ?? 'a command');
    case 'read': return named(verb('Reading', 'Read'), arg(step, 'file_path', 'path', 'notebook_path') ?? 'a file');
    case 'edit': return named(verb('Editing', 'Edited'), arg(step, 'file_path', 'path', 'notebook_path') ?? 'a file');
    case 'write': return named(verb('Writing', 'Wrote'), arg(step, 'file_path', 'path') ?? 'a file');
    case 'search': {
      const what = arg(step, 'pattern', 'query');
      const where = arg(step, 'path');
      return `${verb('Searching', 'Searched')}${what === null ? '' : ` for ${what}`}${where === null ? '' : ` in ${where}`}`;
    }
    case 'web': return named(verb('Fetching', 'Fetched'), arg(step, 'url', 'query') ?? 'a page');
    case 'plan': return step.plan.length === 0 ? verb('Updating the plan', 'Updated the plan')
      : `${verb('Updating the plan', 'Updated the plan')}: ${counts(step.plan)}`;
    case 'subagent': return named(verb('A helper is working on', 'A helper worked on'), arg(step, 'description') ?? 'a task');
    case 'mcp': return named(verb('Using', 'Used'), step.tool.replace(/^mcp__/, '').replace('__', ': '));
    case 'other': return named(verb('Using', 'Used'), step.tool);
  }
}

function took(step: StepItem): string | null {
  if (step.endedAt === null || step.endedAt < step.startedAt) return null;
  const ms = step.endedAt - step.startedAt;
  return ms < 1_000 ? `${ms} ms` : `${(ms / 1_000).toFixed(1)} s`;
}

function outcome(step: StepItem): string | null {
  if (step.status === 'running') return null;
  const parts: string[] = [];
  if (step.status === 'failed') parts.push(step.exit === null ? 'Failed' : `Failed, exit code ${step.exit}`);
  if (step.note !== null) parts.push(step.note.charAt(0).toUpperCase() + step.note.slice(1));
  const time = took(step);
  if (time !== null) parts.push(time);
  return parts.length === 0 ? null : parts.join(' · ');
}

function Lines({ text, sign }: { readonly text: string; readonly sign: '-' | '+' }) {
  return <>{text.split('\n').map((line, index) => <span className="step-diff-line" data-sign={sign} key={index}>
    <span aria-hidden="true" className="step-diff-sign">{sign}</span>{line}{'\n'}</span>)}</>;
}

function repeatsSummary(step: StepItem, item: StepArg): boolean {
  return step.edit !== null && item.key === 'file_path' && !item.cut && item.value === firstLine(item.value);
}

function Body({ step }: { readonly step: StepItem }) {
  const result = step.resultBytes === null ? null
    : step.resultBytes === 0 ? 'No output.' : `${step.resultBytes} bytes of output.`;
  return <div className="step-body">
    {step.kind === 'plan' ? <ul className="step-plan" aria-label="Plan">
      {step.plan.map((item, index) => <li key={index} data-plan-status={item.status}>
        <span aria-hidden="true" className="step-plan-mark">{PLAN_MARKS[item.status]}</span>
        {' '}<span>{item.text}</span><small> · {PLAN_WORDS[item.status]}</small>
      </li>)}
    </ul> : null}
    {step.args.length === 0 ? null : <dl className="step-args">
      {step.args.filter((item) => !repeatsSummary(step, item)).map((item) => <div key={item.key}>
        <dt>{KEY_WORDS[item.key] ?? item.key}</dt>
        <dd><pre className="step-text">{item.value}</pre>{item.cut ? <small className="step-cut">Cut to fit.</small> : null}</dd>
      </div>)}
    </dl>}
    {step.edit === null ? null : <div className="step-diff" role="group" aria-label="Changes">
      <pre className="step-text">{step.edit.old === null ? null : <Lines text={step.edit.old} sign="-" />}<Lines text={step.edit.new} sign="+" /></pre>
      {step.edit.cut ? <small className="step-cut">Cut to fit.</small> : null}
    </div>}
    {step.withheld === null ? null : <p className="step-withheld">Text is hidden: {step.withheld}.</p>}
    {result === null ? null : <p className="step-result">{result}</p>}
  </div>;
}

function StepBox({ step, children, depth }: { readonly step: StepItem; readonly children: ReactNode; readonly depth: number }) {
  const result = outcome(step);
  return <li className="step" data-step-kind={step.kind} data-step-status={step.status} data-step-depth={depth}>
    <details>
      <summary>
        {step.status === 'failed' ? <><span className="step-status">Failed</span>{' '}</> : null}
        <span className="step-summary">{stepSummary(step)}</span>
        {result === null ? null : <small className="step-outcome"> · {result}</small>}
      </summary>
      <Body step={step} />
    </details>
    {children}
  </li>;
}

function Tree({ steps, parent, byParent, depth, seen }: {
  readonly steps: readonly StepItem[]; readonly parent: string | null;
  readonly byParent: ReadonlyMap<string | null, readonly StepItem[]>; readonly depth: number; readonly seen: ReadonlySet<string>;
}) {
  const level = byParent.get(parent) ?? [];
  if (level.length === 0 || depth > MAX_DEPTH) return null;
  return <ol className={depth === 0 ? 'step-list' : 'step-list step-children'}>
    {level.map((step) => seen.has(step.id) ? null : <StepBox key={step.id} step={step} depth={depth}>
      <Tree steps={steps} parent={step.id} byParent={byParent} depth={depth + 1} seen={new Set([...seen, step.id])} />
    </StepBox>)}
  </ol>;
}

/** The agent's steps for one turn, oldest first, a helper's steps under the step that started it. */
export function Steps({ steps, truncated, stopped }: {
  readonly steps: readonly StepItem[]; readonly truncated: boolean; readonly stopped: boolean;
}) {
  if (steps.length === 0) return null;
  const ids = new Set(steps.map((step) => step.id));
  const byParent = new Map<string | null, StepItem[]>();
  for (const step of steps) {
    const key = step.parent !== null && ids.has(step.parent) && step.parent !== step.id ? step.parent : null;
    byParent.set(key, [...(byParent.get(key) ?? []), step]);
  }
  const latest = steps.at(-1);
  return <section className="chat-steps" aria-label="Steps"><details open={steps.length <= OPEN_UP_TO}>
    <summary className="chat-steps-title">Steps <small>({steps.length}){steps.length > OPEN_UP_TO && latest !== undefined
      ? ` · latest: ${stepSummary(latest)}` : ''}</small></summary>
    {truncated ? <p className="chat-warning">Earlier steps are not shown.</p> : null}
    <Tree steps={steps} parent={null} byParent={byParent} depth={0} seen={new Set()} />
    {stopped ? <p className="chat-warning" role="status">This page stopped receiving steps because there were too many.
      Select the agent again to start over.</p> : null}
  </details></section>;
}
