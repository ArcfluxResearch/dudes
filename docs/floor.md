# The Floor

The Floor is the browser view that `dudes floor open` opens. The daemon's state decides where each
agent stands in the office:

- While a turn runs, the agent types at its desk.
- Between turns, it stands at the goal board while the daemon runs the goal's completion check.
- An agent that waits for your answer sits in the lounge under a question mark. An agent on Hold,
  or one that a ceiling or its hop cap stopped, sits there too.
- When the daemon's own run of the completion check passes, the goal is Done and the agent rings
  the bell. What an agent reports about its own work never rings it.

A new hire walks in through the door, and a retired agent walks out. While every agent is paused,
the office dims. Click an agent in the office or in **Crew** to open the drawer, which shows its
goal and status and the Floor actions you turned on, such as answering its question, pausing it or
changing its goal. The drawer opens to a conversation and a message box.
You can send the first message without assigning a goal, watch the reply arrive, and send a
follow-up. A message sent during a turn waits for that turn to finish. Each message to an agent
without a goal authorizes one turn; messages to an agent with a goal join its next task.

![Animation in the Workshop office. A new hire appears at the door, walks across the floor and sits down at a free desk, while the rest of the crew types at desks and checks at the goal board.](../.github/readme/hire-walks-in.webp)

The office is a view of the daemon's state, never a source of truth. The daemon's journal, an append-only log of everything it records, is the
only record, and the Floor changes that state only through the Floor actions you turn on.

The Floor draws the smallest office that seats the whole crew. The Studio has 8 desks, the Workshop
has 12 and HQ has 16. **Settings** can pin one office in this browser instead. When the office has
fewer desks than the crew, the agents without a desk queue by the door, and any past the queue
stand along the back wall. From left to right, the Studio with eight agents, the Workshop with
twelve and HQ with sixteen:

<p>
  <img src="../.github/readme/office-studio.webp" width="32%" alt="The Studio, the smallest office, with eight agents. Four type at desks, two stand by the goal board, one sits on the lounge sofa under a question mark and one stands at the bell.">
  <img src="../.github/readme/office-workshop.webp" width="32%" alt="The Workshop, with twelve agents. Seven type at desks in three clusters, three stand close together at the goal board with their name tags shortened to their names, one sits in the lounge under a question mark and one stands at the bell.">
  <img src="../.github/readme/office-hq.webp" width="32%" alt="HQ, the largest office, with sixteen agents. Most type at desks in four clusters, a few walk between their desks and the goal board, two sit on the lounge sofa under question marks and one stands at the bell.">
</p>

## Open the Floor

`dudes floor open` opens the Floor in your browser, and `dudes quickstart` does the same when it
finishes. It writes a one-time sign-in link into a file only you can read in the runtime directory,
and the system's own opener opens that file: `/usr/bin/open` on macOS. The link works once and
expires after 60 seconds. Over SSH, on Linux and Windows for now, or when the opener fails, it
prints the link instead, and `dudes floor open --no-browser` always prints it.

**Setup** shows each engine the daemon turned on and whether it found that engine's program on its
`PATH`. **Settings** shows the running Floor switches, budget ceilings, per-turn reserves,
registered providers and enrolled workspace labels. The Floor reads configuration without editing
it. **About**, within Settings, shows the daemon, Floor build and browser wire versions.

## After you upgrade

After upgrading Dudes, copy any unsent drafts before reloading an open Floor tab. The daemon and
Floor bundle must have matching release and browser protocol versions. An older open tab cannot
connect to an incompatible daemon. Reload the tab explicitly to load the installed bundle.

## Serve the Floor

The daemon serves the Floor at `/floor/` on `127.0.0.1`. It takes the Floor bundle from a `floor`
directory beside the `duded` executable, where `cargo xtask dist` puts it, or else, when the
executable sits in a directory named `bin`, from `share/dudes/floor` beside that directory, where
the install script puts it. On Unix, the daemon refuses to start with a `share/dudes/floor` that
neither you nor root owns, or that its group or others can write. A `duded` started directly, not
through `dudes quickstart`, can name another bundle with `DUDES_FLOOR_ASSETS`, which takes
precedence, for example `DUDES_FLOOR_ASSETS="$PWD/floor/dist" cargo run -p duded` in a checkout.
With none of these, the Floor listener is disabled, and `dudes floor open --no-browser` says what
to do.

`DUDES_FLOOR_ASSETS`, when present, must name an existing absolute release-bundle directory;
relative and empty paths stop startup. `DUDES_FLOOR_PORT` applies to either bundle and must be
unsigned ASCII decimal that fits `u16`; `0` asks the operating system for a free port, and a taken
port stops startup. Without it, the daemon first asks for the port its last run bound, which it
keeps in a `floor-port` file in the state directory, and takes a free port when that one is taken.
The Floor's address then stays the same across restarts, so the roster order a
browser keeps for it survives `dudes stop` and `dudes quickstart`. The startup line and
`dudes floor open --no-browser` both show the port chosen. `dudes quickstart` passes neither
variable to the daemon. Dudes always binds `127.0.0.1`; neither variable can change the host.

The release manifest proves that the directory is internally complete and version-matched. It does
not prove where those files came from or grant them access to daemon data. A bundle that fails the
manifest check stops startup, whichever way it was found. The path and port are non-secret and appear
in startup diagnostics, so do not put credentials in either value.

