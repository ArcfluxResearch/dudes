// SPDX-License-Identifier: MIT OR Apache-2.0

import type { JournalStopped } from './journal-stopped.ts';
import { journalPosition } from './format.ts';

export function JournalStoppedBanner({ stopped, className = 'journal-stopped-banner' }: {
  readonly stopped: JournalStopped | null;
  readonly className?: string;
}) {
  if (stopped === null) return null;
  return <div className={className} role="alert">
    <p>Dudes cannot save changes. Check the disk for free space or errors, then restart Dudes.</p>
    <details><summary>Details</summary><p>Saving stopped at {journalPosition(stopped.lsn)}.</p></details>
  </div>;
}
