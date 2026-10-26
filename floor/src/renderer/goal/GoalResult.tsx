// SPDX-License-Identifier: MIT OR Apache-2.0

import { Disclosure } from '../../ui/index.ts';
import { journalPosition } from '../format.ts';
import type { GoalResultView } from './result.ts';

const TEXT = 'm-0 text-base text-ink';
const TERM = 'text-base font-normal normal-case tracking-normal text-muted';

export function GoalResult({ result }: { readonly result: GoalResultView | null }) {
  if (result === null) return null;
  return <section className="grid gap-1.5" aria-label="Latest completion check">
    <h3 className="m-0 font-sans text-base font-semibold text-ink">Latest completion check</h3>
    {result.kind === 'missing' ? <p className={TEXT}>No completion check result reported</p>
      : result.kind === 'unavailable' ? <p className={TEXT}>Completion check result unavailable</p>
        : <>
          <p className={TEXT}>{result.kind === 'not_run'
            ? 'Latest check could not run. Its program did not start, so this result says nothing about the task. The journal shows why (dudes tail).'
            : result.outcome === 'passed' ? 'Latest check passed' : 'Latest check failed'}</p>
          <dl className="m-0 grid gap-1 text-base empty:hidden">
            {result.kind === 'not_run' ? null : <div className="flex flex-wrap gap-x-3 border-0 p-0"><dt className={TERM}>Who ran it</dt><dd className="m-0">{result.kind === 'observed'
              ? 'Dudes ran it' : 'The agent reported this. Dudes did not run it, so treat it as unproven'}</dd></div>}
          </dl>
          <Disclosure summary="Details">
            <dl className="m-0 flex flex-wrap gap-x-3 text-base"><div className="contents"><dt className={TERM}>Journal position</dt><dd className="data m-0 font-mono text-small">{journalPosition(result.position)}</dd></div></dl>
          </Disclosure>
        </>}
  </section>;
}
