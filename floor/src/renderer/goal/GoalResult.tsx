// SPDX-License-Identifier: MIT OR Apache-2.0

import { journalPosition } from '../format.ts';
import type { GoalResultView } from './result.ts';

export function GoalResult({ result }: { readonly result: GoalResultView | null }) {
  if (result === null) return null;
  return <section aria-label="Latest completion check">
    <h3>Latest completion check</h3>
    {result.kind === 'missing' ? <p>No completion check result reported</p>
      : result.kind === 'unavailable' ? <p>Completion check result unavailable</p>
        : <>
          <p>{result.kind === 'not_run'
            ? 'Latest check could not run. Its program did not start, so this result says nothing about the goal. The journal shows why (dudes tail).'
            : result.outcome === 'passed' ? 'Latest check passed' : 'Latest check failed'}</p>
          <dl>
            {result.kind === 'not_run' ? null : <div><dt>Who ran it</dt><dd>{result.kind === 'observed'
              ? 'Dudes ran it' : 'The agent reported this. Dudes did not run it, so treat it as unproven'}</dd></div>}
          </dl>
          <details><summary>Details</summary>
            <dl><div><dt>Journal position</dt><dd className="data">{journalPosition(result.position)}</dd></div></dl>
          </details>
        </>}
  </section>;
}
