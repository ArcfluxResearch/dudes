# The Floor

The Floor is the browser view that `dudes open` opens. The daemon's state decides where each
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
only record, and the Floor changes that state only through the Floor actions you turn on. `dudes quickstart` turns on hire, pause, answer, change
goals, retire, recover, tickets and add projects without asking. The changed-file views are on too. The terminal
stays off until you turn it on with `dudes config set floor.terminal on` and restart.

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

`dudes open` starts the daemon when it is down, mints a one-time sign-in link and opens it in your
default browser (`open` on macOS, `xdg-open` on Linux). `dudes quickstart` ends by doing the same,
and the installer adds a Dudes launcher that runs it: an app in `~/Applications` on macOS, an entry in
your applications menu on Linux. The launcher registers no web address and no file type, so no web page
can start it. `dudes uninstall` removes it. Over SSH, or where no opener is found, `dudes open` prints
the link instead, to paste into a browser on this computer within 60 seconds.
`dudes floor open` always prints the link and opens nothing. Because you carry that link to another
browser yourself, it works for 10 minutes. It has its own slots, and the daemon keeps the last four it
printed, so printing a new one drops the oldest unused one and never stops `dudes open` from making its own.

A link works once, in one browser. The page signs in with it as it loads and
removes it from the address bar and the browser's history. The same link used in a second browser, or
after it expired, is refused. The page then tells you to open Dudes again. Copying the address from
the browser's address bar does not help, because the link is gone from it. The link is a loopback address, so on a remote machine forward the port first, for example `ssh -L <port>:127.0.0.1:<port> <host>`, and paste the link into a browser on your own computer.

### Signed-in browsers

Each browser that signs in becomes a *device*: the daemon writes it to its journal with a name the
browser gives itself, such as "Chrome on macOS". The name is a label the browser chose, not proof of
what it is. `dudes devices` lists every signed-in browser, with when it signed in, when it first and
last made a request, and a `new` mark on one nobody has kept yet. `dudes devices remove <id>` signs
one out and closes its open connections at once; the start of the id is enough when it names one
browser, and when it names more than one the command lists them and removes nothing. An id is 43
characters of upper and lower case letters, digits, `-` and `_`, and the start you give must match
exactly, including case. An id that starts with `-` needs `--` before it:
`dudes devices remove -- -Ab3...`.
A new browser stays marked `new`, and stays early in the removal order below, until someone keeps
it. The Floor's **Keep**, pressed in another browser, does that. `dudes devices keep <id>` does the
same from the terminal, for when you have no second browser.

The Floor shows the same list under **Settings**, marks the browser you are using, and has a **Sign
out** button on each row. A new sign-in is announced on every other open Floor, with **Keep** and
**Remove**, and the notice stays until someone answers it. The signed-out screen says why a browser was
signed out when the daemon can tell it.

A device stays signed in for 30 days without use and 90 days in all. It stays signed in through
`dudes stop` and `dudes start`, because the journal holds it, as long as the Floor comes back on the
same port. If the Floor starts on another port, for example because its usual port is busy, the
browsers signed in on the old port are signed out until the daemon is back on that port; they are not
removed, and they work again when it returns. A device that signs in and makes no request within 24 hours is removed. You can also end a
device with **Sign out**, which closes its open connections, or with `dudes devices remove`.

### How the Floor knows a request is yours

The browser makes a key for itself when it signs in, and the page cannot read that key: the page can
only ask the browser to sign with it, and it cannot copy it out. The sign-in sends the daemon the
public half. The browser's id is a hash of that public key, so it is the same everywhere the daemon
shows it. The browser's cookie only names the device. It opens nothing on its own: every request
the page makes, including each read, each action and each connection the page opens, carries a signature
made with the key. The signature covers the request, the page's address, the time and a one-time
number, and the daemon refuses a request whose signature is missing, is for another address, is more
than a minute off the daemon's clock, or has been used before. A page served by another program on the
same port, even one the browser sent your cookie to, has no key, so it can open nothing and cannot
sign for you.

A browser needs WebCrypto and IndexedDB for this, and a page never falls back to anything weaker. Some
private windows do not allow IndexedDB, and a page there cannot sign in; it says why. A
browser that clears the site's data loses its key. Its cookie then names a device it can no longer sign
for, and the page asks for a new link with `dudes open`; the old device ages out.

Signing in again after an upgrade from a version that kept a readable secret is needed once: those
sign-ins hold no key, so the first daemon with signed requests ends each of them as expired, and every
browser shows the signed-out screen and the `dudes open` line.

One limit stays. If another program held the Floor's address, served its own page there and
registered a service worker, that worker stays in the browser after the program leaves and can
keep acting inside that browser when Dudes returns to the address. The daemon answers `404` for every script it does not
serve, which makes a browser drop such a worker over time, and the page removes any worker it finds. If
you think another program held the address, clear that site's data in the browser: cookies,
storage and service workers for `127.0.0.1` and the port.

The daemon keeps at most eight devices. A ninth sign-in removes the first of these that exists: a
device that never made a request, a device nobody has kept yet, then the device used longest ago. The
browser it removes shows the signed-out screen. A page whose device the daemon no longer knows shows
the same screen, and a page whose connection merely dropped keeps its device and reconnects.

**Setup** shows each engine the daemon turned on and whether it found that engine's program on its
`PATH`. When you hire and the engine you chose is installed but signed out, the hire flow says so and
offers **Sign in**. It runs that engine's own login in a terminal on the page: for Claude Code,
open the link in your browser and paste the code it shows into the terminal. Codex finishes by itself
once you sign in at its link. Pi signs in inside its own terminal, where you type `/login`. The page
checks the engine again when the login exits cleanly and the hire carries on. This follows
`floor.hire` and needs no terminal switch. Closing the terminal, waiting ten minutes or stopping the
daemon ends the login. Dudes does not keep or log what the terminal shows or what you type, and the
engine writes its own credentials. **Settings** shows the running Floor switches, budget ceilings, per-turn reserves,
registered providers and your projects. The Floor reads configuration without editing
it. **About**, within Settings, shows the daemon, Floor build and browser wire versions.

## Hire an agent

The Hire dialog has three steps: **Who** (a name, and optionally a role and a look), **Project**, and
**App**. A hire needs no task, and it does not need tickets to be on. The new agent waits, idle, at its
desk. With tickets on and none open, the Floor then offers **Give a task** for it.

## Hire an agent into its own copy of the project

Two agents in one folder can overwrite each other. The Hire dialog's **App** step offers **Work in
its own copy of the project** for a Claude Code agent. A copy is a separate checkout of the project
on a new branch named `dudes/<id>`, kept in Dudes' own data folder and not inside your project. When
the agent is retired the copy and its branch stay, so nothing it wrote is lost. Dudes never merges,
pushes or deletes them.

The box starts unticked. It starts ticked, and says why, when another agent already works in the
project's folder. Your own choice wins after that.

The box appears only for a project you have trusted. In a terminal, run `dudes project trust <id>`
(`dudes project untrust <id>` takes it back). Trusting records the project's folder, its `.claude`
directory and `.mcp.json` at the current commit, and its git configuration. An agent in a copy runs the
repository's own Claude settings, so their environment values and helper commands apply without
asking. If any of those change, or the folder is replaced, copies stop until you trust the project
again. Trust is refused for a repository whose git configuration includes other files or defines a
filter, or whose `.claude` or `.mcp.json` is a symbolic link. Codex and Pi agents cannot work in a copy,
and neither can any agent on Windows. Agents already in a copy keep working after an untrust.

## Pause, stop and resume

With `floor.control` on, an agent's drawer offers **Pause**, **Stop this turn** and **Resume**. Pause lets a
running turn finish and starts no new one. **Stop this turn** ends the running turn at once and keeps
the agent: the page says "Stopping" while the turn's process ends and "Stopped" when no turn is open,
and nothing new starts until you press **Resume**. Stopping ends the turn's whole process tree, so it can
leave partial edits in the working tree and a stale git lock file. The Floor refuses a stop while a
person has the agent's terminal open. **Resume** lifts a stop made here, a pause and an automatic hold; a halt
made with `dudes halt` stays until `dudes resume`. In Chat, **Stop and send** stops the turn, sends your
message as a note once the turn has ended and then lifts the stop. This is not the ticket board's
**Stop**, which ends a ticket. See [Pause, resume, halt and retire](goals-and-stopping.md#pause-resume-halt-and-retire).

## Recover an interrupted goal

**Recover interrupted goal** appears in an eligible agent's drawer when both `floor.recovery` and
`floor.control` are on. Press **Inspect recovery**, read the warning, choose one offered context,
and press **Authorize one recovery**. Ordinary Resume does not release a recovery barrier.
Authorization does not guarantee a launch or restored conversation history, and earlier effects may repeat.

Keep the request ID shown in the receipt. If the outcome is unknown, press **Check receipt** while
the current page retains the request. It remains available to a live authenticated Floor reader
when recovery is off. After reloading the page, use `dudes recovery receipt <request-id>` with the
saved ID. Both lookups are read-only and never resubmit recovery. See
[Recover an interrupted goal](goals-and-stopping.md#recover-an-interrupted-goal) for the command-line steps.
If unused consent is invalidated, see
[Make a new choice after unused consent is invalidated](goals-and-stopping.md#make-a-new-choice-after-unused-consent-is-invalidated).

For a ticket, recovery keeps the same goal and round. The ticket review shows recovered attempts
and whether the last recorded check applies to the current work. After later work is admitted,
an earlier check is history. Unknown applicability is not a pass.

## Tickets

With `floor.board` on, the Board view makes tickets and moves them. It needs `floor.control` and
`floor.goal` on too, and the daemon refuses to start without them. Turn it on with
`dudes config set floor.board on`, which applies at once. `dudes quickstart` turns it on with the
other Floor actions in a new file.

**Giving a ticket to an agent starts that agent, which then runs and spends under your account.**
Anyone holding your Floor session can do that until the session ends, so keep the link to yourself.
Set a spending limit when your account bills per use.

- **New ticket.** A title is all it needs. The note, the project, the "Done when" check and the
  agent are optional. The project is asked for only when more than one is enrolled. With no agent
  hired, the form says so and offers to hire one.
- **The board.** To do, Working, Ready for you and Done, and a Needs you row for tickets that are
  stuck. Done shows the newest 20. A card shows the title, the agent, its place in the queue or why
  it waits, and in Needs you the sentence that names the next step.
- **A ticket.** Open a card to read its note, what the agent says it did, the check result and the
  files that changed. The agent's words are shown as plain text. The files come from the Git tab,
  so they need `floor.git` on.
- **Changes.** With `floor.git` and `floor.diff` both on, a started ticket also has a Changes view.
  It lists the files that differ from where the agent's round began, including new files that Git
  does not track yet, and opens one file's old and current text side by side. A new file shows as
  Added with an empty old side. Files that Git ignores, in `.gitignore` or `.git/info/exclude`, are
  neither listed nor counted. Files that are binary, too large, hard-linked, named like a
  credential file, or that look like they hold a secret are listed without their text. A folder
  that holds its own Git repository is listed and never opened. If more files changed than fit,
  tracked changes take the places first, and the page counts the new files it could not list. If
  files already existed untracked when the round began, the page warns that some new files may
  not be the agent's. A label says "Unchanged since the check" or "Changed since the check" by comparing the
  checkout with what Dudes sampled just after the ticket's check ran. It says "Unknown" whenever
  that sample is missing or incomplete, and "Unknown" is not a pass. Nothing is refreshed on its
  own: Refresh makes a new listing. Quickstart turns both on in a new file; an older file may have them off, and **Settings** turns them on. Each session can
  read about one mebibyte in all. When it is shown, it replaces the Changed files list above.
- **Actions.** Give to agent, Edit and Remove for a ticket nobody has started. Stop for one that is
  Working. Accept, Ask for changes and Remove for one that is Ready for you. Try again, Stop and
  Remove for one in Needs you. Remove for one that is Done, to clear it off the board. Accept is your verdict that the work is what you asked for. The check
  passing only means the daemon's own run passed.
- **Banner.** The top bar says how many tickets are ready for you or need you, and how many agents
  working a ticket are asking you a question. Overview lists the tickets under Waiting on you. A
  turn ending never raises it. While a ticket waits for you, its agent's crew pill and the top
  bar's count read "ready for you" or "needs you" instead of "done", and the old goal-done notice
  is not shown for it.

[Tickets](tickets.md) covers the same actions from the command line.

## After you upgrade

After upgrading Dudes, copy any unsent drafts before reloading an open Floor tab. The daemon and
Floor bundle must have matching release and browser protocol versions. An older open tab cannot
connect to an incompatible daemon. Reload the tab explicitly to load the installed bundle.

Signed-in browsers stay signed in across `dudes upgrade`, `dudes stop` and `dudes start`, so a reload
is all it takes. When the daemon stops, an open tab reconnects for about eight seconds, then stops
sending anything that carries your sign-in and checks every two seconds, without it, whether Dudes
is back. After an upgrade it says "Dudes was updated. Reload this tab to continue." After the reload
the tab is signed in again and shows the same agents, with no new link. Each device keeps its own
time limits. Ticket and check allowances carry over through `dudes upgrade`, so an upgrade does not
refill them. A plain `dudes stop` and `dudes start` starts them again.

A tab that finds no daemon for ten minutes stops checking and shows the sign-in message. A browser
signs in again if the Floor comes back on a different port, because the sign-in belongs to the
address it was made on.

The first upgrade from a version that kept sign-ins only in memory still signs open tabs out once,
because that daemon has no devices to hand over. Open Dudes again with `dudes open`.

Do not paste a new sign-in link into a tab that says Dudes stopped answering. Reload the tab first.

One sign-in serves one tab at a time. If you opened a second tab with the same sign-in, it asks you
to sign in again after the reload.

## Serve the Floor

The daemon serves the Floor at `/floor/` on `127.0.0.1`. It takes the Floor bundle from a `floor`
directory beside the `duded` executable, where `cargo xtask dist` puts it, or else, when the
executable sits in a directory named `bin`, from `share/dudes/floor` beside that directory, where
the install script puts it. On Unix, the daemon refuses to start with a `share/dudes/floor` that
neither you nor root owns, or that its group or others can write. A `duded` started directly, not
through `dudes quickstart`, can name another bundle with `DUDES_FLOOR_ASSETS`, which takes
precedence, for example `DUDES_FLOOR_ASSETS="$PWD/floor/dist" cargo run -p duded` in a checkout.
With none of these, the Floor listener is disabled, and `dudes open` says what
to do.

`DUDES_FLOOR_ASSETS`, when present, must name an existing absolute release-bundle directory;
relative and empty paths stop startup. `DUDES_FLOOR_PORT` applies to either bundle and must be
unsigned ASCII decimal that fits `u16`; `0` asks the operating system for a free port, and a taken
port stops startup. Without it, the daemon first asks for the port its last run bound, which it
keeps in a `floor-port` file in the state directory, and takes a free port when that one is taken.
The Floor's address then stays the same across restarts, so the roster order a
browser keeps for it survives `dudes stop` and `dudes start`, and it is what keeps your devices
signed in. The startup line and
`dudes open` both show the port chosen. `dudes quickstart` and `dudes start` pass
neither variable to the daemon. Dudes always binds `127.0.0.1`; neither variable can change the host.

The release manifest proves that the directory is internally complete and version-matched. It does
not prove where those files came from or grant them access to daemon data. A bundle that fails the
manifest check stops startup, whichever way it was found. The path and port are non-secret and appear
in startup diagnostics, so do not put credentials in either value.

