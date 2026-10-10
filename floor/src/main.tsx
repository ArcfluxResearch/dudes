import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { connect, noteFirstLinkRefused, noteNoFirstLink, noteSignInExpired, takePastedLink } from './connect';
import type { StreamPorts } from './connect';
import { noteTransport } from './renderer/floor/store';
import { browserSocket } from './renderer/floor/stream';
import { LinkRefused, currentFloorSession, establishFloorSession, reauthenticateFloorSession } from './session';
import { WebFloorShell } from './shell';
import './style.css';

/** The browser's own socket, beside the page that authenticated, on the browser's clock. */
const BROWSER: StreamPorts = { location: window.location, socket: browserSocket };

// Opened here, once, rather than from a component: a session admits two live
// sockets and a double-mounted effect would take both for one view. A link
// pasted while this is still in flight may settle first; this result is then
// stale, and it neither opens nor closes anything.
const openedWithLink = window.location.hash.startsWith('#bootstrap=');
void establishFloorSession()
  .then((session) => {
    if (session !== currentFloorSession()) return;
    if (session === null) {
      if (openedWithLink) noteNoFirstLink();
      else noteSignInExpired();
      noteTransport('close');
      return;
    }
    connect(session, BROWSER);
  })
  .catch((error: unknown) => {
    if (currentFloorSession() !== null) return;
    if (error instanceof LinkRefused) noteFirstLinkRefused();
    noteTransport('close');
  });

// A fresh Floor link pasted into this tab differs from the open page only in
// its fragment, so the browser delivers it as a hashchange and does not reload.
// It is exchanged here, in place, so that a Hire or an answer this page holds
// in memory survives the session it was sent under. A link the daemon refuses
// leaves the page as it was, and the page says so.
window.addEventListener('hashchange', () => {
  void takePastedLink(reauthenticateFloorSession, BROWSER);
});

const root = document.getElementById('root');
if (root === null) throw new Error('Floor root is missing');

createRoot(root).render(
  <StrictMode>
    <WebFloorShell />
  </StrictMode>,
);
