// SPDX-License-Identifier: MIT OR Apache-2.0

import type { JournalStopped } from './journal-stopped.ts';
import { journalPosition } from './format.ts';

export function JournalStoppedBanner({ stopped, className = 'journal-stopped-banner' }: {
  readonly stopped: JournalStopped | null;
  readonly className?: string;
}) {
  if (stopped === null) return null;
  return <p className={className} role="alert">
    The journal stopped accepting writes at {journalPosition(stopped.lsn)}; likely a full disk or an I/O error; free space and restart the daemon.
  </p>;
}
