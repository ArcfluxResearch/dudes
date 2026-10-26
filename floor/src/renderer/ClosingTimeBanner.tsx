// SPDX-License-Identifier: MIT OR Apache-2.0

import { useEffect, useState } from 'react';
import type { ClosingProgress, ClosingTime } from './closing-time.ts';
import { cn } from '../ui/index.ts';
import { NoticeIcon, noticeLine } from './notices/look.tsx';

export function ClosingTimeBanner({ closing, progress = null, stale }: {
  readonly closing: ClosingTime; readonly progress?: ClosingProgress | null; readonly stale: boolean;
}) {
  const [clock, setClock] = useState(() => ({ progress, receivedAt: performance.now(), now: performance.now() }));
  if (clock.progress !== progress) {
    const now = performance.now();
    setClock({ progress, receivedAt: now, now });
  }
  useEffect(() => {
    if (stale || closing !== 'announced' || progress === null) return;
    const timer = window.setInterval(() => setClock(previous => ({ ...previous, now: performance.now() })), 1000);
    return () => window.clearInterval(timer);
  }, [stale, closing, progress]);
  if (closing === null) return null;
  const remaining = progress === null ? null : Math.ceil(Math.max(0, progress.remaining_ms - (clock.now - clock.receivedAt)) / 1000);
  const early = closing === 'complete' && progress !== null && progress.owed === 0 && progress.running === 0 && progress.remaining_ms > 0;
  return <div className={cn('closing-time-banner', noticeLine('warn'))}>
    <p className="m-0" role="status" aria-atomic="true">
    <NoticeIcon tone="warn" /><strong className="font-semibold">{stale ? 'Last observed: ' : ''}{closing === 'announced' ? 'Closing time announced' : 'Closing phase complete'}</strong>
    {' '}{early ? 'Completed early.' : closing === 'announced' ? 'Dudes is coordinating closing.' : 'Closure is complete.'}
    {progress !== null && <> {progress.acknowledged} of {progress.acknowledged + progress.settled + progress.owed} acknowledged, {progress.settled} settled, {progress.owed} still owed, {progress.running} {progress.running === 1 ? 'turn' : 'turns'} still running. Settled means nothing left to save.</>}
    </p>
    {remaining !== null && closing === 'announced' && <p className="closing-time-countdown m-0 text-small text-muted">{stale ? 'Last observed estimate: ' : ''}About {remaining} {remaining === 1 ? 'second' : 'seconds'} left.</p>}
  </div>;
}
