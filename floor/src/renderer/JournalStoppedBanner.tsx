// SPDX-License-Identifier: MIT OR Apache-2.0

import type { JournalStopped } from './journal-stopped.ts';
import { journalPosition } from './format.ts';
import { Disclosure, cn } from '../ui/index.ts';
import { NoticeIcon, noticeLine } from './notices/look.tsx';

export function JournalStoppedBanner({ stopped, className = 'journal-stopped-banner' }: {
  readonly stopped: JournalStopped | null;
  readonly className?: string;
}) {
  if (stopped === null) return null;
  return <div className={cn(className, noticeLine('danger'))} role="alert">
    <p className="m-0 font-medium"><NoticeIcon tone="danger" />Dudes cannot save changes. Check the disk for free space or errors, then restart Dudes.</p>
    <Disclosure summary="Details" className="mt-1 text-small text-muted" summaryClassName="w-fit text-small text-muted">
      <p className="m-0 text-small text-muted">Saving stopped at {journalPosition(stopped.lsn)}.</p></Disclosure>
  </div>;
}
