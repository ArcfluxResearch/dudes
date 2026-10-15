// SPDX-License-Identifier: MIT OR Apache-2.0

import { currentFloorSession, floorSessionGeneration } from '../../session.ts';
import { floorFetch } from '../../transport.ts';
import { refreshFloorSnapshot } from '../floor/stream.ts';
import { postDiff } from './diff.ts';
import type { DiffTransport } from './diff.ts';

export const browserDiffTransport: DiffTransport = {
  async request(request) {
    const outcome = await postDiff(request, currentFloorSession()?.csrf ?? null, floorFetch);
    // An empty 403 may mean the session ended or that this page can no longer act with it;
    // a fresh subscription tells the two apart.
    if (outcome.kind === 'forbidden') refreshFloorSnapshot();
    return outcome;
  },
  generation: () => floorSessionGeneration(),
};
