// SPDX-License-Identifier: MIT OR Apache-2.0

import { ArrowLeft } from 'lucide-react';
import type { ReactNode } from 'react';

import { Button, ICON } from '../../ui/index.ts';
import { CopyLink } from './CopyLink.tsx';

/** One ticket filling the Board's area: Back to the view it came from, and the link that opens it again. */
export function TicketPage({ id, onBack, children }: {
  readonly id: string;
  readonly onBack: () => void;
  readonly children: ReactNode;
}) {
  return <section className="ticket-page mx-auto flex w-full max-w-[56rem] flex-col gap-3" aria-label="Ticket page">
    <div className="ticket-page-bar flex flex-wrap items-center gap-2">
      <Button size="sm" icon={<ArrowLeft {...ICON} />} className="ticket-back" aria-keyshortcuts="Escape" onClick={onBack}>Back</Button>
      <CopyLink id={id} />
    </div>
    <div className="ticket-page-body rounded-lg border border-line bg-surface">{children}</div>
  </section>;
}
