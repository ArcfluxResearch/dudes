# Floor

The Floor is the browser view the daemon serves at `/floor/`. It shows the daemon's agents, board
and activity, and the daemon's journal stays the only record of them. Screenshots of the office,
**Crew** and the drawer are in the project documentation under [The Floor](../docs/floor.md).

During closing time, the banner shows how many participants acknowledged, how many are settled
(nothing left to save), how many still owe an acknowledgement, and how many still have running
turns. The time left is an estimate bounded by the daemon's saving interval. A disconnected view
keeps these facts as **Last observed** and freezes its countdown until a fresh snapshot arrives.
Completion shows the final counts; a new daemon run does not show a historical closing banner.

Use Node 24.21.0 and npm 11.19.0. Run `npm ci --ignore-scripts`, `npm run check:licenses`,
`npm run typecheck`, then `npm test` (which builds before inspecting output). Generated files live in `dist/` and are not
committed. `scripts/write-release-manifest.mjs` inventories the build with SHA-256 and moves Vite's
bundled dependency notices to a content-hashed, manifest-admitted asset.
