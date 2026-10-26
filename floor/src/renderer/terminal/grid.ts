// SPDX-License-Identifier: MIT OR Apache-2.0
//
// The grid and its stylesheet, as one module the panel imports only when a
// terminal opens. A Floor with the terminal off fetches neither.

import './xterm.css';

export { Terminal } from '@xterm/xterm';
