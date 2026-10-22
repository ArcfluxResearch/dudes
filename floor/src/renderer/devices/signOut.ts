// SPDX-License-Identifier: MIT OR Apache-2.0

import { showSignedOut } from '../../connect.ts';
import { currentFloorSession, signOutFloorSession } from '../../session.ts';
import { noteSignOutFailed } from './devices.ts';

/**
 * Sign this browser out: the daemon ends the session first, and only when it confirms does the page
 * drop its secret and show the signed-out page. Anything else leaves the browser signed in, and the
 * page says so.
 */
export async function signOutHere(): Promise<void> {
  const session = currentFloorSession();
  if (session === null) return;
  noteSignOutFailed(false);
  if (await signOutFloorSession(session)) showSignedOut();
  else noteSignOutFailed(true);
}
