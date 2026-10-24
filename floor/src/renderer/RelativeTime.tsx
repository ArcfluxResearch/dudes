// SPDX-License-Identifier: MIT OR Apache-2.0

import { useSyncExternalStore } from 'react';
import { recordedTime, relativeTime } from './format.ts';

let now = Date.now();
let timer: ReturnType<typeof setInterval> | undefined;
const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  if (timer === undefined) {
    now = Date.now();
    timer = setInterval(() => {
      now = Date.now();
      for (const notify of listeners) notify();
    }, 60_000);
  }
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0) {
      clearInterval(timer);
      timer = undefined;
    }
  };
}

const currentTime = () => now;

export function RelativeTime({ at, className }: { readonly at: number | string; readonly className?: string }) {
  const now = useSyncExternalStore(subscribe, currentTime, currentTime);
  const date = new Date(at);
  return <time className={className} dateTime={Number.isNaN(date.getTime()) ? undefined : date.toISOString()}
    title={recordedTime(at)}>{relativeTime(at, now)}</time>;
}
