// SPDX-License-Identifier: MIT OR Apache-2.0

import { Check, LoaderCircle, X } from 'lucide-react';
import type { ReactNode } from 'react';
import { Button, Disclosure, ICON, RING, cn } from '../../ui/index.ts';
import { requestStepResult } from '../floor/store.ts';
import { EMPTY_RESULTS, hasKeptText } from '../floor/steps.ts';
import type { PlanItem, ResultText, StepArg, StepItem, StepResult } from '../floor/steps.ts';

const SUMMARY_CHARS = 100;
const PLAN_MARKS = { pending: '☐', in_progress: '▸', completed: '☑' } as const;
const PLAN_WORDS = { pending: 'to do', in_progress: 'in progress', completed: 'done' } as const;
const MAX_DEPTH = 8;
const OPEN_UP_TO = 8;
const KEY_WORDS: Readonly<Record<string, string>> = {
  command: 'Command', file_path: 'File', path: 'Folder', notebook_path: 'Notebook', pattern: 'Search for', query: 'Search for',
  cwd: 'Working folder', move_path: 'Moved to', url: 'Address', description: 'Task', prompt: 'Instructions', subagent_type: 'Helper type',
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

const TEXT = 'step-text m-0 overflow-x-auto rounded-md bg-surface-2 px-2 py-1.5 font-mono text-small leading-normal whitespace-pre-wrap text-ink [overflow-wrap:anywhere]';
const NOTE = 'block text-small text-muted';

function Lines({ text, sign }: { readonly text: string; readonly sign: '-' | '+' }) {
  return <>{text.split('\n').map((line, index) => <span key={index} data-sign={sign}
    className={cn('step-diff-line block', sign === '-' ? 'bg-danger-soft' : 'bg-ok-soft')}>
    <span aria-hidden="true" className="step-diff-sign me-1 inline-block w-3 text-muted select-none">{sign}</span>{line}{'\n'}</span>)}</>;
}

function repeatsSummary(step: StepItem, item: StepArg): boolean {
  return step.edit !== null && item.key === 'file_path' && !item.cut && item.value === firstLine(item.value);
}

type Results = Readonly<Record<string, StepResult>>;

function CutNote({ text }: { readonly text: ResultText }) {
  return text.cut ? <small className={cn('step-cut mt-1', NOTE)}>Cut to fit. Only the start of the output is kept.</small> : null;
}

function Diff({ old, next, cut, note }: {
  readonly old: string | null; readonly next: string; readonly cut: boolean; readonly note: string;
}) {
  return <div className="step-diff" role="group" aria-label="Changes">
    <pre className={cn(TEXT, 'max-h-72 overflow-y-auto')}>{old === null ? null : <Lines text={old} sign="-" />}<Lines text={next} sign="+" /></pre>
    {cut ? <small className={cn('step-cut mt-1', NOTE)}>{note}</small> : null}
  </div>;
}

/** What the daemon kept for the step: its output, or an edit too long to show. Asked for once, on opening. */
function Kept({ step, kept }: { readonly step: StepItem; readonly kept: StepResult | undefined }) {
  const edit = step.kind === 'edit' || step.kind === 'write';
  if (kept === undefined) {
    return <Button size="sm" className="step-show mt-1.5 self-start" onClick={() => requestStepResult(step.id)}>
      {edit ? 'Show the full text' : 'Show the output'}</Button>;
  }
  switch (kept.state) {
    case 'loading': return <p className={cn('step-result m-0', NOTE)} role="status">Loading…</p>;
    case 'waiting': return <p className={cn('step-result m-0', NOTE)}>This step is still running.</p>;
    case 'gone': return <p className={cn('step-result m-0', NOTE)}>This text is no longer kept.</p>;
    case 'withheld': return <p className={cn('step-withheld m-0', NOTE, 'text-warn')}>Output is hidden: {kept.reason ?? 'it may hold a secret'}.</p>;
    case 'refused': return <p className={cn('step-withheld m-0', NOTE, 'text-warn')}>This output is unavailable: {kept.reason ?? 'the request was refused'}.</p>;
    case 'ready':
      if (kept.edit !== null) {
        return <Diff old={kept.edit.old?.text ?? null} next={kept.edit.new.text}
          cut={kept.edit.new.cut || kept.edit.old?.cut === true} note="Cut to fit. The whole change is in the Changes view." />;
      }
      return kept.result === null ? <p className={cn('step-result m-0', NOTE)}>No output.</p> : <div className="step-output">
        <pre className={cn(TEXT, 'max-h-72 overflow-y-auto', RING)} tabIndex={0}>{kept.result.text}</pre><CutNote text={kept.result} /></div>;
  }
}

function Body({ step, results }: { readonly step: StepItem; readonly results: Results }) {
  const result = step.resultBytes === null ? null
    : step.resultBytes === 0 ? 'No output.' : `${step.resultBytes} bytes of output.`;
  const kept = hasKeptText(step) ? results[step.id] : undefined;
  const whole = kept?.state === 'ready' && kept.edit !== null;
  return <div className="step-body flex flex-col gap-2 pb-2">
    {step.kind === 'plan' ? <ul className="step-plan m-0 list-none p-0" aria-label="Plan">
      {step.plan.map((item, index) => <li key={index} data-plan-status={item.status}
        className={cn('my-0.5', item.status === 'completed' && 'text-muted [&>span:not(.step-plan-mark)]:line-through')}>
        <span aria-hidden="true" className="step-plan-mark">{PLAN_MARKS[item.status]}</span>
        {' '}<span>{item.text}</span><small className="text-muted"> · {PLAN_WORDS[item.status]}</small>
      </li>)}
    </ul> : null}
    {step.args.length === 0 ? null : <dl className="step-args m-0 flex flex-col gap-2">
      {step.args.filter((item) => !repeatsSummary(step, item)).map((item) => <div key={item.key} className="flex flex-col gap-1">
        <dt className="text-caption font-semibold tracking-label text-muted uppercase">{KEY_WORDS[item.key] ?? item.key}</dt>
        <dd className="m-0"><pre className={TEXT}>{item.value}</pre>{item.cut ? <small className={cn('step-cut mt-1', NOTE)}>Cut to fit.</small> : null}</dd>
      </div>)}
    </dl>}
    {step.edit === null || whole ? null
      : <Diff old={step.edit.old} next={step.edit.new} cut={step.edit.cut} note="Cut to fit." />}
    {step.withheld === null ? null : <p className={cn('step-withheld m-0', NOTE, 'text-warn')}>Text is hidden: {step.withheld}.</p>}
    {result === null ? null : <p className={cn('step-result m-0', NOTE)}>{result}</p>}
    {hasKeptText(step) ? <Kept step={step} kept={kept} /> : null}
  </div>;
}

function StatusMark({ status }: { readonly status: StepItem['status'] }) {
  if (status === 'running') return <LoaderCircle {...ICON} aria-hidden="true" className="shrink-0 animate-spin text-info" />;
  if (status === 'failed') return <X {...ICON} aria-hidden="true" className="shrink-0 text-danger" />;
  return <Check {...ICON} aria-hidden="true" className="shrink-0 text-ok" />;
}

function StepBox({ step, children, depth, results }: {
  readonly step: StepItem; readonly children: ReactNode; readonly depth: number; readonly results: Results;
}) {
  const result = outcome(step);
  return <li className="step my-1" data-step-kind={step.kind} data-step-status={step.status} data-step-depth={depth}>
    <Disclosure className="rounded-md border border-line bg-surface px-2" summaryClassName="py-1.5 font-normal"
      onToggle={(event) => { if (event.currentTarget.open && hasKeptText(step)) requestStepResult(step.id); }}
      summary={<>
        <StatusMark status={step.status} />
        <span className="step-summary min-w-0 flex-1 truncate group-open:whitespace-normal group-open:[overflow-wrap:anywhere]">{stepSummary(step)}</span>
        {result === null ? null : <small className={cn('step-outcome shrink-0 text-caption text-muted',
          step.status === 'failed' && 'text-danger')}> · {result}</small>}
      </>}>
      <Body step={step} results={results} />
    </Disclosure>
    {children}
  </li>;
}

function Tree({ steps, parent, byParent, depth, seen, results }: {
  readonly steps: readonly StepItem[]; readonly parent: string | null;
  readonly byParent: ReadonlyMap<string | null, readonly StepItem[]>; readonly depth: number; readonly seen: ReadonlySet<string>;
  readonly results: Results;
}) {
  const level = byParent.get(parent) ?? [];
  if (level.length === 0) return null;
  if (depth > MAX_DEPTH) return <p className="chat-warning step-deeper m-0 text-small font-semibold">Deeper steps are not shown.</p>;
  return <ol className={cn('step-list m-0 list-none p-0', depth > 0 && 'step-children ms-4 border-s-2 border-line ps-2')}>
    {level.map((step) => seen.has(step.id) ? null : <StepBox key={step.id} step={step} depth={depth} results={results}>
      <Tree steps={steps} parent={step.id} byParent={byParent} depth={depth + 1} seen={new Set([...seen, step.id])} results={results} />
    </StepBox>)}
  </ol>;
}

/** The agent's steps for one turn, oldest first, a helper's steps under the step that started it. */
export function Steps({ steps, truncated, stopped, results = EMPTY_RESULTS }: {
  readonly steps: readonly StepItem[]; readonly truncated: boolean; readonly stopped: boolean; readonly results?: Results;
}) {
  if (steps.length === 0) return null;
  const ids = new Set(steps.map((step) => step.id));
  const byParent = new Map<string | null, StepItem[]>();
  for (const step of steps) {
    const key = step.parent !== null && ids.has(step.parent) && step.parent !== step.id ? step.parent : null;
    byParent.set(key, [...(byParent.get(key) ?? []), step]);
  }
  const latest = steps.at(-1);
  return <section className="chat-steps text-base" aria-label="Steps">
    <Disclosure open={steps.length <= OPEN_UP_TO} summaryClassName="chat-steps-title font-semibold"
      summary={<span>Steps <small className="font-normal text-muted">({steps.length}){steps.length > OPEN_UP_TO && latest !== undefined
        ? ` · latest: ${stepSummary(latest)}` : ''}</small></span>}>
      {truncated ? <p className="chat-warning m-0 text-small font-semibold">Earlier steps are not shown.</p> : null}
      <Tree steps={steps} parent={null} byParent={byParent} depth={0} seen={new Set()} results={results} />
      {stopped ? <p className="chat-warning m-0 text-small font-semibold" role="status">This page stopped receiving steps because there were too many.
        Reload the page to start over.</p> : null}
    </Disclosure>
  </section>;
}
