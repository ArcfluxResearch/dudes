# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Maintainers prepare released sections from Conventional Commits before manually tagging a release.
`cargo xtask release-notes --draft --from <previous tag>` prints a draft. The release workflow
refuses a tag whose version has no section here, and builds the GitHub Release body from it.

## [Unreleased]

## [0.102.0] - 2026-10-31

### Highlights

- **A Claude Code agent can now be hired into a project made of several repositories.** List the repositories once with `dudes project repositories <project> --primary <id> --repo <id>=<path> ...`, then hire with `dudes hire --project <project> --worktree --trusted` or the Hire dialog. The agent gets its own copy of every repository, works in the primary copy, and has the other copies added to its session. Chat, mail and ticket turns all start that way. (#339)
- **A ticket in such a project always covers every repository.** The ticket lists the primary repository first and the others after it, and its review shows a diff for each repository. A ticket that names only some of the repositories is refused. (#327, #339)
- **Commits count as changes.** The default "files changed" check used to look only for uncommitted changes, so a round whose work was all committed counted as having changed nothing. In a multi-repository project it now also passes when any copy holds a commit made since the start of the round. (#333)
- **Instruction files are reviewed in every copy.** A skill, command or agent file changed in any hired copy during a ticket round holds the round for your review, as it does for a single-repository agent. (#338)
- **Recovery, attach and chat work as they do for a single-repository agent.** After a crash or restart, an interrupted turn is issued again once, in the primary copy with the other copies added, and a copy that has gone missing holds the agent instead. Open the real CLI starts Claude in the primary copy with the other copies added and watches every copy. (#331, #337, #338)

### What the agent may write and what Claude loads

- **The agent may change any file in any of its own copies.** It can commit there and run commands, under the same permission setting and trust as a single-repository agent in its own copy. Dudes does not confine it to the repositories a ticket names, and every ticket names all of them.
- **Skills, commands and agents load from every copy.** Claude Code offers them from each copy, and Dudes reviews those files in every copy.
- **Memory files, settings, hooks and MCP files load only from the primary copy.** Dudes removes `CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD` from the agent's environment, so another copy's memory files cannot load. (#338)

### Changes

- Groundwork for the above: a policy for hires that may write every copy and a journal record to carry it, the options a single-repository hire takes (model, permission setting, declared servers), launching a turn in the primary copy with the other copies added, and the checks that used to assume one working copy. (#324, #328, #330, #337)
- What stays refused: Codex and Pi agents ("Multi-repository projects work with Claude only for now."), native Windows ("Multi-repository hiring is not available on this platform."), a goal outside a ticket ("A goal outside a ticket is not available for a multi-repository agent yet. Give it a ticket instead."), and a ticket for only some of the repositories. (#327, #337, #339)

### Fixes

- The Floor ticket page showed "Dudes sent a reply this page could not read." and no path list when a round changed an instruction file in an agent's own copy, in a multi-repository or a single-repository project. It now shows the review with the changed paths. (#340)
- On a multi-repository hire, every ticket ended in Needs you and Accept always failed with "Instruction coverage is incomplete in a hired repository", even when the agent changed nothing. A ticket round now reaches Ready for you, Accept works, and an interrupted round recovers and starts again. (#341)

### Breaking

- **The first start after the upgrade rebuilds the saved views once.** The projection version moves from 71 to 73. Nothing is lost, and the rebuild runs once.
- **Going back to 0.101.2 works until you hire into a multi-repository project.** 0.101.2 starts on a 0.102.0 journal that holds no such hire, including one with an agent working in its own copy, and also when it has to rebuild the saved views. It keeps your tickets, notes, priorities, agents and copies, and what you write there is read when you come forward again.
- **After a multi-repository hire, 0.101.2 refuses to start on that data.** The daemon exits with status 1 before it answers or serves the Floor, changes nothing in the journal, and prints `journal required record kind 113 at 1:6426 is not understood`, where the segment and byte position are those of your journal. `dudes start` reports only that the daemon exited. Start 0.102.0 again and everything is intact.

## [0.101.2] - 2026-10-31

### Highlights

- **The selected agent's header is laid out in clear bands.** The top of an agent's panel used to stack a name, a status line, a badge, a sentence, a pause note, mixed buttons and a model line, with the close button floating over the corner. It now has an identity row with the portrait, the name, a muted "role, engine" line, the More menu and the close button. Under it, one status badge is followed by at most one short sentence. A paused agent reads "Paused" and says what starts it again, instead of showing "Needs you" next to a second "Paused". A held agent says why in a line and ends with what to do, for example "Claude Code did not start, usually because claude is not installed. Install it and sign in, then press Resume." (#334)
- **One row of actions, always in the same order.** The action the state calls for comes first (Answer, Review, Open ticket or Resume), then Talk, Give a task, Pause or Resume, Stop this turn and Open the real CLI. Give a task opens its form in a panel under the row. (#334)
- **The model is one line you click to change.** "Model: name" opens the model editor right below it, and a small chevron shows that it can be changed. A model fixed in the configuration shows as plain text. (#334)
- **The tabs stay in reach.** The header no longer scrolls away with the tab contents. Only the panel under the tabs scrolls, so Overview, Chat and the other tabs stay reachable on a short window or a phone. (#334)

### Changes

- More groundwork for tickets that span several repositories: a project made of several repositories now gets one catalog record in the journal. A project with one repository never gets one, and nothing offers multi-repository work yet. (#326)

### Fixes

- When the office cannot draw, its notice no longer makes the page taller than a short window that has an agent's panel open beside it. (#334)

### Breaking

- **The first start after the upgrade does not rebuild the saved views.** The projection version stays at 71, as in 0.101.1.
- **Going back to 0.101.1 works.** 0.101.1 starts on a 0.101.2 journal, including one with an agent working in its own copy, and also when it has to rebuild the saved views. It keeps your tickets, notes, priorities, agents and copies, and what you write there is read when you come forward again.

## [0.101.1] - 2026-10-30

### Highlights

- **A faster New ticket dialog.** The dialog now starts with the title. A row of chips sets the priority, the Done when check and the agent. Each agent in the list says in plain words what it is doing, for example "Idle, starts now" or "Queue full (8 of 8)", and whether it works in its own copy or the shared folder. One agent is marked "Suggested", and Dudes never picks for you. A line above the buttons says what Create will do, for example "Ada starts it now, in its own copy." or "Goes to To do, 3rd by priority." (#329)
- **The dialog helps you before you create.** After three characters, up to three open tickets in the same project that share your words appear with an Open button, so you can spot a duplicate. Pasting several lines into an empty title puts the first line in the title and the rest in the note, and Cmd+Z (Ctrl+Z) undoes the split. Cmd+Enter (Ctrl+Enter) creates the ticket from anywhere in the dialog, including the note. (#329)
- **Less retyping.** A "Create more" switch keeps the dialog open and keeps the project, check, priority and agent for the next ticket. A draft kept with Escape now keeps its priority and says "Draft kept." when you reopen the dialog. The dialog opens on the project and agent chosen in the Board filters, and the To do column has a small + that opens it. (#329)
- **Limits are stated before you hit them.** The title counts bytes against its 256-byte limit and says "Shorten it." when it is over. Create stays off with the reason printed beside it. The first-task form on Setup and in the agent drawer checks the same limit. (#329)

### Changes

- More groundwork for tickets that span several repositories: frozen scoped ticket authority, stricter recovery checks for a hire that spans repositories, the hire dialog's repository group, per-repository changes in the Git tab and the ticket Changes view, and capturing changes and running checks in each hired copy. Nothing offers multi-repository work yet, and a journal without these records replays unchanged. (#312, #315, #316, #317, #325)

### Fixes

- A daemon that runs for weeks no longer keeps a process handle and a file descriptor for every turn it has finished. (#321)

### Breaking

- **The first start after the upgrade rebuilds the saved views once.** The projection version moves from 70 to 71, so Dudes rebuilds its saved views from the journal. Nothing is lost. (#315)
- **Going back to 0.101.0 works.** 0.101.0 starts on a 0.101.1 journal, including one with an agent working in its own copy. It keeps your tickets, notes, priorities, agents and copies, and what you write there is read when you come forward again. (#315)

## [0.101.0] - 2026-10-30

### Highlights

- **Tickets run in separate copies at the same time.** Agents that each work in their own copy of a project now run their tickets in parallel. Agents that share the project folder still take turns. A ticket that has to wait names the ticket holding its own working copy, not any ticket in the project. (#293)
- **A ticket shows its own copy.** A ticket whose agent works in its own copy shows the copy's branch, whether the copy is in use or released, and buttons that jump to the ticket's Changes view and the agent's Git view. Doing and Review cards carry a small branch chip. The hire dialog now ticks "Work in its own copy of the project" by default for a Claude hire into a trusted project. (#287)
- **Ticket notes read as Markdown, and you write them in a rich editor.** A note shows headings, lists, code blocks, small tables, bold, italic and links. Only `http` and `https` links are live. Editing a note, and writing one on the New ticket form, opens an editor with Markdown shortcuts. A note the editor cannot hold exactly, such as one with a table or an image, opens in a plain Markdown box instead, so nothing is rewritten without you editing it. The 8,192-byte limit is now counted in bytes, so a long note is refused before you send it. (#292, #295)
- **Unassigned tickets start by priority, then age.** On the Board, in the List and in `dudes ticket list`, unassigned To do tickets sort Urgent, High, then Medium and unranked together, then Low, and the oldest comes first within each. (#288)
- **Set priority and read a note from a file on the command line.** `dudes ticket add` and `dudes ticket edit` take `--priority urgent|high|medium|low|none`, and `--note-file <path>` (`-` for standard input) so a longer note does not have to fit in one shell argument. (#285, #289)
- **Attach opens Claude on the model the agent uses.** Before, the terminal opened on Claude's default model even when the agent ran on another. The confirmation says which model it opens on. (#290)

### Changes

- The journal and daemon gain more groundwork for tickets that span several repositories: repository trust pins, named completion-check repositories, per-repository instructions, hires across repository copies and scoped launch preparation. No command or Floor screen offers multi-repository work yet, and a journal without these records replays unchanged. (#269, #270, #272, #273, #297, #298, #299, #300, #301, #302, #303, #304, #306, #307, #313)

### Fixes

- After you Halt or Stop an agent's running turn, hiring another agent into the same folder without its own copy is no longer refused until the daemon restarts. The daemon now records that the stopped turn's processes are gone. (#314)
- Cleaning up a half-made copy after a failure or a restart now removes only the folder Dudes created. If something else has replaced that folder, or Dudes cannot tell, it leaves the folder alone and leaves the branch for you. (#296)
- A ticket handed to another agent while a question is open now shows every open question, each headed by the agent that asked it, instead of only the newest. (#286)

### Breaking

- **The first start after the upgrade rebuilds the saved views once.** Dudes rebuilds the board and repository views from the journal. Nothing is lost. (#269, #296)
- **Going back to 0.100.0 works until you hire an agent into its own copy on 0.101.0.** Every such hire now records a new kind of entry that 0.100.0 cannot read. 0.100.0 then refuses to start, names the entry (`journal required record kind 112 at 1:467 is not understood`), exits with status 1 and leaves the journal as it was, so a later 0.101.0 start picks up where you left off. Without a hire like that, 0.100.0 starts normally, keeps your tickets and priorities, and takes new writes that 0.101.0 reads when you come forward. (#296)

## [0.100.0] - 2026-10-28

### Highlights

- **Find and browse tickets on the Board.** The Board gains a List layout beside the columns, a search box, and filters for project, agent, status and priority. The List can group its rows. Your layout, filters and grouping are remembered. (#274, #282)
- **Move around the Board from the keyboard, and open a ticket as a page.** `/` searches, `c` starts a new ticket, `j` and `k` (or the arrow keys) move through the visible tickets, `b` and `l` switch layout, and `?` lists the shortcuts. Cmd+K on macOS or Ctrl+K elsewhere opens a command menu with New ticket, Open Board, Open List and your tickets by title. A ticket's **Full page** button fills the Board area with it, **Back** returns to the same layout, filters and scroll position, and **Copy link** gives an address that opens the ticket. (#276)
- **Rank tickets with a priority and read a ticket's history.** A ticket can be Urgent, High, Medium or Low. A project's queue starts the Urgent ones first, then High, then Medium and unranked together by age, then Low. Board cards and List rows show a priority mark, and the new-ticket, edit and give forms and a quick control on a To do ticket set it. A ticket's page and dialog list its newest 48 events, newest first, and say how many older ones they leave out. (#280, #282)
- **See an agent's question on its ticket.** When an agent asks a question while it works a ticket, the ticket shows the open question with the answer form under it, and the history records the asking and the answering. A ticket that is not Working says the answer goes to the agent's next work. With answering off the question is read-only and names the setting. (#283)
- **A Projects page, and new projects from the Floor.** Projects shows a card for each project with its agents and tickets, a button that opens its Board, and Add project. With `floor.project.create_parent` set, the Add project dialog also offers **Create a new project**, which needs only a name, makes the folder and reports what became of it, including the exact path when a failure leaves one behind. (#278, #275)

### Changes

- The journal now records repository sets, per-repository trust, declared ticket repository authority, review baselines and copy hires. This is groundwork for tickets that span several repositories. No command or Floor screen offers it yet, and existing journals replay unchanged. (#262, #263, #265, #266, #267, #268)

### Fixes

- With about twelve or more agents the Floor no longer scrolls past the bottom of the window to reach the last roster label. (#256)
- A window exactly 800 pixels wide no longer scrolls to over twice its height, and a phone-sized office stays inside the window. (#277)
- The sign-in banner stays inside the Hire, Retire and Overview dialogs instead of covering the top bar. (#281)

### Breaking

- **The first start after the upgrade rebuilds the saved views once.** Dudes rebuilds the board views from the journal. Nothing is lost. (#280, #283)
- **Going back to 0.99.1 ignores ticket priority.** An older daemon reads the journal, orders every project's queue by age, and shows no priority or history. Starting 0.100.0 again brings them back. (#280)

## [0.99.1] - 2026-10-27

### Fixes

- On a wide screen the Floor no longer scrolls 64 pixels past the bottom of the window into an empty strip. Every view fills the window exactly again, and a phone keeps its room for the roster bar. (#253)

## [0.99.0] - 2026-10-27

### Highlights

- **A redesigned Floor.** Every screen, dialog and control now draws from one design: Inter for text and JetBrains Mono for data, neutral surfaces with one teal accent, owned buttons, fields, selects, menus, switches, tabs, dialogs and disclosures, Lucide icons, and the same focus ring everywhere. Native browser controls and the old hand-written styles are gone, light and dark both meet the 4.5:1 text contrast bar, and the page still runs under the strict style policy. (#236, #237, #238, #239, #240, #241, #242, #243, #248)
- **The Floor fits a phone and keeps the conversation in view.** Phone tap targets are at least 44 pixels high, the header takes two rows, and the office fills the screen. An agent's panel scrolls as one region, so the conversation keeps its height when a form or a receipt is open, and notices no longer cover the message box. Control receipts read as plain status lines, and the reason someone needs you shows on two lines in the roster. (#249)
- **See what a message is about and whether it was answered.** Messages show each mail's subject, cut to 120 bytes, and whether the sender marked it as a message or a request. A request shows Awaiting, Answered, Undeliverable or Thread full, worked out from what Dudes recorded rather than from what an agent says. Dudes removes the secrets it knows about from a subject when the mail is sent and saves only the scrubbed subject, so ending the turn or restarting never reveals the original. (#247)
- **Answering mail from the command line is easier.** `dudes mail list` shows your current mail without reading the bodies, reading a request prints the exact reply command, and a reply can leave out `--subject` to reuse `Re:` and the original subject. (#235)

### Fixes

- A ticket's result and engine exit survive a later Chat message or mail wake after the ticket reached review, and rework starts with a clean exit. (#234)
- Windows uninstall uses the native executable names, and a named-pipe disconnect no longer sends an unanswered control hook down the general fail-open path. (#231)
- Release builds no longer warn about a test-only stream constructor. (#233)
- Timing-dependent admission tests no longer fail at random, and the lockfile moves `crypto-bigint` off a yanked version. (#230)

### Breaking

- **Reload open Floor tabs after the upgrade.** The page and the daemon speak a new browser wire version, so a tab left open from 0.98.0 asks for a reload and keeps its drafts. (#247)
- **The first start after the upgrade rebuilds the saved views once.** Dudes rebuilds the board and mail views from the journal. Nothing is lost. (#234, #247)
- **Mail sent before the upgrade shows no subject on the Floor.** Its subject was saved before scrubbing existed, so the Floor leaves it out. (#247)
- **Going back to 0.98.0 works, with one gap.** 0.98.0 ignores the new subject field. A mail that was part way through delivery to several agents when you downgrade may stay pending until you upgrade again. (#247)

## [0.98.0] - 2026-10-26

### Highlights

- **One roster shows who needs you.** A single list beside the office replaces the Crew list, the strip of cards, the Overview lists and the focus mode roster. Each agent has one row with its face, its name, one state word, one line about what it is doing and the button for what it needs from you. The same state word appears in the office badges, the top bar counts and the Overview dialog, and the heading says in plain words who needs you. (#220, #222)
- **An agent's panel opens on its state.** The panel now starts with the agent's name, its state, one sentence about it and the main action, then Pause or Stop and the model. Retire, Edit look and Open the real CLI moved into a menu. A finished agent no longer shows its old goal as current work. (#222)
- **See why a turn failed and which model ran.** When a chat turn fails, Chat shows the engine's own words under "The engine said" instead of "No final reply was reported". The agent panel shows the model the latest turn ran. Dudes also asks Codex and Pi which models they offer, and Codex streaming now works with codex-cli 0.164 as well as 0.160. (#223)
- **Agents know who they are.** Every task a managed agent receives now carries its name, its role and its id, so asking "Who are you?" gets the right answer. `dudes whoami` prints the same facts, and `dudes team` marks the asking agent's own row with `(you)`. (#221)
- **Open an agent's real CLI without a restart.** Turning on the terminal in Settings, or with `dudes config set floor.terminal on`, now applies at once and does not end running work. The Attach button sits beside the model, and for Codex and Pi agents it explains that opening their own terminal is not supported yet instead of failing. (#227)
- **Read code in agent replies with colour.** Fenced code in chat replies and in a ticket's summary is highlighted for TypeScript, JavaScript, JSON, Rust, Python, shell and diff. Other languages keep the plain block. (#217)

### Floor

- Notices float over the bottom corner of the office instead of reserving a strip, so the office, the Board and every sheet use the full height. Notices that must stay in view, such as Dudes being unable to save, sit in a thin banner under the top bar. (#212)
- The ticket review sheet is wider on large screens, so a wide code block no longer needs sideways scrolling. Idle agents walk to different spots in the office, and an agent whose work is accepted leaves the bell. (#213)
- Setup, answers, memory and retirement use plain words, and a ticket no longer repeats a generic warning for an engine's standing limits. Specific limits stay in the ticket's details. (#219)

### Fixes

- Starting several tickets at once no longer holds some of them for instruction review because the daemon had not looked at their folders. Every start is now sampled in order. (#226)
- Pressing Stop before a turn has started now ends that turn's live output, so the page shows "Last turn ended". An agent with nothing run since Dudes started now reads "Idle" instead of "Waiting". (#228)
- Steps no longer offer an output control for a connected-server tool, an unknown tool or a background shell, whose output can never be shown. (#216)
- The release notes no longer say a release has no new capabilities when its Highlights list some. (#215)
- Timing-dependent tests no longer fail at random on a busy machine. (#214, #224, #225)

### Breaking

- **Reload open Floor tabs after the upgrade.** The page and the daemon speak a new browser wire version, so a tab left open from 0.97.1 asks for a reload. (#223)
- **Going back to 0.97.1 is refused once the journal holds a new record.** The first Chat message, completed-goal note or mail wake that a managed agent takes after the upgrade writes a record that 0.97.1 cannot read, and 0.97.1 then refuses to start and names the record. Until one is written, you can still go back. Back up the state directory first if you might. (#221)
- **The first start after the upgrade rebuilds the saved view once.** Dudes rebuilds the saved view of chat turns from the journal. Nothing is lost. (#223)

## [0.97.1] - 2026-10-25

### Fixes

- When Pi fails a turn, for example because its default model is not available for the signed-in account or a model name matches more than one provider, Chat now shows Pi's own error instead of "No final reply was reported". (#209)

## [0.97.0] - 2026-10-25

### Highlights

- **See what an agent is doing.** Chat shows a Steps list for an agent's latest turn, one collapsed line per command, read, edit, search, plan, web request or helper, with its status and how long it took. Claude Code, Codex and Pi agents all show steps. Open a finished step to see its output as plain text, with known secrets redacted and long output cut to fit. (#198, #199, #202)
- **Stop one turn without losing the agent.** With Floor control on, an agent's drawer offers Stop this turn next to Pause. It ends the running turn and keeps the agent. In Chat, Stop and send stops the turn, then sends your message once the agent is idle. (#196)
- **Change an agent's model without hiring it again.** `dudes model <agent> <name>` sets the model for the agent's next turn, and `dudes model <agent> --clear` returns it to the engine's own model. The agent keeps its session and history, and the Floor shows the current and next model. (#195)
- **See what the last turn used.** A selected agent shows the tokens and dollars of its last turn and, for Claude, how much of the model's context window it filled. A ticket shows the steps its newest turn took. (#200)
- **Secrets stay out of agent text.** Dudes now removes the secrets it knows about from an agent's replies, final replies, questions and ticket summaries before it saves them, and the Floor shows `(redacted)` in their place. Memory entries and the text in the Changes view are saved as written, and the Floor scrubs them only when it shows them, using the secrets Dudes holds at that moment. An agent's own environment secrets are held only while the agent runs. Mail is saved as written and the Floor never shows it. (#194)
- **Read agent replies as formatted text.** Agent summaries and chat replies show headings, lists, quotes, tables, code and emphasis. Links appear as text with their full address and are never clickable, and raw HTML and images stay as plain text. (#190)

### Floor

- Notices sit in one fixed area, so they no longer move the agent drawer or dialogs. Desktop notifications are offered once on the Board. (#187)
- The real CLI is easy to find: the selected agent shows Open the real CLI, or explains how to turn it on. (#191)
- The hire flow is simpler. The first-agent card is centered, a name is required, an engine is picked automatically when only one is available, and there is one way to dismiss. (#192)
- Actions and status read in plain language. The Board has one create action and each ticket one Give to agent action, times stay current while the page is idle, and the header is reduced to Hire, Pause all and More. (#193)
- Pause, stop, live work and hire messages use plain words instead of internal terms. (#201)
- A ticket drawer shows the ticket's current round and waiting line, and marks a reading that may be out of date. (#186, #189)
- Show changes gives the real reason when it cannot show a diff, and no longer offers Refresh when Refresh cannot help. A Done ticket whose agent was retired is explained. (#186, #188, #189)

### Fixes

- Five timing-dependent tests no longer fail at random on a busy machine. (#197, #204)

### Breaking

- **Reload open Floor tabs after the upgrade.** The page and the daemon speak a new browser wire version, so a tab left open from 0.96.0 asks for a reload. (#195)
- **Going back to 0.96.0 is refused once the journal holds a new record.** A Floor Stop (#196) and a model change from `dudes model` or the Floor (#195) each write a record that 0.96.0 cannot read. Until one of them is written, you can still go back.
- **The first start after the upgrade rebuilds the saved view once.** Dudes rebuilds the saved view of agents, tickets and turns from the journal. Nothing is lost. (#196, #200)

## [0.96.0] - 2026-10-24

### Highlights

- **Set up the Floor from the page.** The first-run guide offers the existing Floor switches directly and no longer repeats the first-task step. When adding a project, hiring, or Pi sign-in needs a switch, the page points to the matching setting. An exhausted goal offers Start a new goal, distinct from giving an agent a ticket. (#182)
- **Clear the daily dollar ceiling in Settings.** Clear asks for confirmation and removes the Floor's daily dollar ceiling after a restart. Until then, the page that cleared it shows "No daily dollar ceiling after restart" beside the running amount. Reloaded pages and other tabs show the running amount without the note. Per-turn reserves cannot be cleared and must stay above zero. (#183)
- **Find command help without scrolling through every command.** `dudes --help` is a short command index, with detailed usage and flags under each command's own `--help`. Quickstart help describes engine sign-in status and the Floor's sign-in offer. (#180)

### Fixes

- Test helpers have a deadline so a killed test runner cannot leave them running indefinitely. Git-reader safety tests use smaller, shared fixtures while keeping their refusal checks and positive controls. (#181)
- `crypto-bigint` is updated to 0.7.6 and `ctutils` to 0.4.5 for RUSTSEC-2026-0387. (#185)

## [0.95.0] - 2026-10-24

### Highlights

- **Review shows the files an agent created.** A ticket's Changes view now lists new files as Added, not only tracked files that differ, and opens each one side by side with an empty old side. Files Git ignores, through `.gitignore` or `.git/info/exclude`, are neither listed nor counted, so the Git tab's untracked count stops counting build output. A folder that holds its own Git repository is listed and never opened. If files were already untracked when the round began, the page says some new files may not be the agent's. (#174)
- **The changed-file views are on after `dudes quickstart`.** A new config turns on the Git and Changes views with the other Floor actions, so Ready for you lists the changed files without a trip to Settings. A config that already chose its Floor actions is left as it is. (#169)
- **Hire without a first task.** The Hire dialog asks who, which project and which app. The new agent waits at its desk, and the Floor's Give a task card offers it work. Hiring no longer needs tickets on; with tickets off, the last step says so and offers the switch. (#170)
- **Choose the model when hiring.** The Hire dialog's App step offers the engine's default, suggestions for Claude Code (`opus`, `sonnet`, `haiku`) and a box for any other name, and `dudes hire --model <name>` does the same. The model is fixed for the agent's life. The daemon checks only the name's shape, so a name the engine does not know fails the agent's first turn with the engine's own error. When Claude's `extra_args` set `--model`, a hire that names a model is refused and the dialog shows the configured one. (#173)
- **Give a task from the Board, and keep drafts.** A one-line Give a task form on the Board gives an idle agent work. New ticket and Ask for changes keep their text across closes and reloads until a send is confirmed. (#175)

### Changes

- After Accept, the ticket says where the changes are, in the project or in the agent's own copy, and that Dudes did not commit or push them. The Changes view stays on the Done ticket, so its files can still be opened side by side. (#178)
- The first ticket you give offers "Tell me when it's ready?" once, and one click turns on desktop notifications. (#175)
- Settings is grouped by job, with configuration keys and support commands under Details. Ticket, Chat, Hire, Changes and Log text uses plain words instead of command lines, and record positions and identifiers move under Details. (#177)
- Chat's **Clear all** removes an agent's queued notes after one confirmation. Notes already sent, or whose delivery is unknown, stay. (#177)
- Trying again after a lost answer to a new ticket gives back the ticket the first try made, instead of making a second card. (#167)
- The installer adds a Dudes launcher that runs `dudes open`: an app in `~/Applications` on macOS and an applications-menu entry on Linux. A launcher that cannot be written never fails the install, and `dudes uninstall` removes it. (#128)
- The Changes view withholds the text of a file with more than one hard link, because its bytes may belong to a file outside the project. It also withholds more files whose names or contents look like secrets, such as `id_ed25519`, `.npmrc`, `*.tfvars` and URLs that carry a password. (#174)

### Fixes

- `ctutils` is updated to 0.4.4 for RUSTSEC-2026-0385. (#172)

### Breaking

- **Reload open Floor tabs after the upgrade.** The page and the daemon speak browser wire version 6, so a tab left open from 0.94.0 asks for a reload. (#173)
- **`dudes hire --model` needs a 0.95.0 daemon.** An older daemon refuses the request. Upgrade the daemon and the command line together, as the installer does. (#173)
- **Going back to 0.94.0 drops a hire's model.** Every agent is kept, but an agent hired with a model then runs on the configured or engine default model. (#173)

## [0.94.0] - 2026-10-23

### Highlights

- **`dudes quickstart` asks nothing.** It detects your engines, turns on what the Floor needs, writes the config, starts the daemon and opens the Floor. On a config with no Floor action on yet it turns on answering, hiring, pausing and resuming, goals, retiring, the board, renewal, recovery and projects. The git and diff views and the terminal stay off, and the summary says where to turn them on. It sets no spending cap, adds no project and hires nobody unless an option asks; you add a project and hire on the Floor. (#159)
- **Chat sends files.** Paste a screenshot, drop a file or press **+** in Chat. Dudes keeps a copy in the agent's own folder and adds its path to the message, so the agent opens it with its own tools. A file is at most 10 MiB and an agent's folder at most 200 MiB, and the folder is deleted when the agent retires. Claude agents are given read access to that folder. Files work with agents Dudes launches, not with attached ones. (#164)
- **A hire's first task is a ticket.** The task you give in the Hire dialog now appears on the board and moves through To do, Working, Ready for you and Done like any other card. A project whose check already passes can be hired into. (#162)
- **Choose a project folder with your system's folder window.** Add project and Hire show **Choose folder…** where the daemon can open one: on macOS, and on Linux with a display and `zenity` or `kdialog`. Typing a path still works everywhere, and nothing is offered over SSH or on Windows. (#163)
- **Log, Setup and Settings are pages.** Each has its own address (`#/log`, `#/setup`, `#/settings`), works with Back, reload, Office and Escape, and keeps live updates. (#161)

### Changes

- An agent's Overview opens on one line that says what it is doing now, with the one action that fits, then anything that needs you, then its current work with the latest check. Output and the details (engine, mode, project, turns, cost and id) are folded away, and Edit look moves to the sheet's header. (#166)
- The "Give a task" prompt starts as one line for all free agents, does not take focus, and closes with its close button or Escape until the set of free agents changes. (#160)
- Chat keeps its message box at the bottom of the sheet, with the conversation scrolling above it, on desktop and phones. (#160)
- Re-running `dudes quickstart` keeps every Floor setting of a config that already turns one on, applies Floor switch and project changes to a running daemon at once, and restarts the daemon only when no agent is busy. When one is, it names the agents, prints the restart commands and exits 4. (#159)
- A ticket waiting for review with the git view off points to **Show changed files** in Settings instead of a terminal command. (#159)

### Breaking

- **Reload open Floor tabs after the upgrade.** The page and the daemon speak a new browser wire version, so a tab left open from 0.93.0 asks for a reload. (#162)
- **Going back to 0.93.0 or earlier after hiring from the Floor is not supported.** A Floor hire made by 0.94.0 is recorded without a goal, and earlier daemons stop with a "journal corrupt" error at that record. Nothing is lost: starting 0.94.0 again reads the journal as before. (#162)

## [0.93.0] - 2026-10-22

### Highlights

- **The Floor signs every request with a key the page cannot read.** When a browser signs in, it makes a key it cannot export and keeps it in IndexedDB, and the daemon keeps only the public half. Every read, action and connection the page makes carries a signature made with that key, so a copied cookie opens nothing. A browser needs WebCrypto and IndexedDB; one without them is told so on the sign-in screen. Checked in Chrome, Firefox and Safari, in normal and private windows. (#156)
- **See and end the browsers signed in to the Floor.** Settings lists each signed-in browser by name, with when it signed in and was last used, and a Sign out button. Every other open Floor announces a new sign-in with Keep and Remove, and a signed-out page says why: you signed out, another browser or the terminal removed it, it expired, or a newer sign-in replaced it. In the terminal, `dudes devices` lists them, `dudes devices remove <id>` signs one out and closes its connections, and `dudes devices keep <id>` keeps a new one. (#150, #151)
- **Sign-ins last longer and survive restarts.** A browser stays signed in for 30 days without use and 90 days in all, instead of 24 hours and 30 days, and stays signed in across `dudes stop`, `dudes start` and `dudes upgrade`. The daemon keeps up to eight browsers. A ninth sign-in removes one that was never used, else one nobody has kept, else the one used longest ago. (#150)

### Changes

- `dudes open` in a browser that is already signed in keeps it as the same browser, instead of adding another one. (#156)
- A link printed by `dudes floor open` lives 10 minutes and has its own slot, so printing one never blocks `dudes open`, whose links live 60 seconds. (#150)
- A held ticket's Review opens the interrupted work instead of Chat, and the recovery panel uses plainer words. (#157)
- The README opens with the Floor, a screenshot and a two-command quick start, and links a demo that needs no account. Upgrade details moved to Get started. (#146, #155)

### Fixes

- A ticket whose engine turn failed shows the exit code or signal and the time on its card and in its detail. (#152)
- Notices show over the full-screen Setup, Settings, Log and agent sheets on a phone, and several at once no longer cover most of the screen. (#149)
- Notices no longer make the Hire and Retire buttons unreachable or cover the first-task input. (#153)
- On a phone, the open agent panel no longer covers the receipt a held ticket points to. (#157)

### Breaking

- **Every browser signs in once more.** Browsers that signed in before this release hold no key, so the first 0.93.0 daemon ends each of them once. Run `dudes open` to sign in again. A page left open across the upgrade says the daemon stopped or restarted without its sign-in. (#156)

## [0.92.0] - 2026-10-21

### Highlights

- **Run Dudes from the Floor, with no terminal.** A signed-in Floor now acts for you. Add a project from a folder path and remove it in Settings, change the Floor's switches, engines and spending limits in Settings, sign an engine in from the Hire dialog, page through finished tickets, and resume or review a held ticket's recovery. Floor switches apply to a running daemon at once, with no restart. (#123, #124, #125, #129, #131, #132, #134, #135)
- **`dudes open` signs you in.** It starts the daemon if it is down, makes a one-time sign-in link and opens it in your browser. Over SSH it prints the link instead. A sign-in renews while you use it, stays signed in while its tab is on screen, and ends after 24 hours idle, after 30 days, or when you choose Sign out. At four sign-ins, a fifth ends the one used longest ago. (#127, #138)
- **Agents cannot act through the Floor.** Every Floor request passes one check, and a request from inside an agent's processes is refused and recorded. Before the page sends its secret, it checks that it is talking to the real daemon. (#121, #137)
- **Two agents in one project, each in its own copy.** Trust a project with `dudes project trust <id>`, then tick "Work in its own copy of the project" when you hire. The agent works on its own branch in a separate checkout, so two agents do not edit the same files. (#133)

### Changes

- The Floor, the command line and the docs say "project" everywhere, and `dudes workspace` is now `dudes project`. (#120, #139)
- `dudes project add` in a repository with no test command uses the files-changed check, as quick start does. (#120)
- `dudes quickstart` on a new install turns on the Floor's actions and leaves the browser terminal off. A config file that already chooses its own switches is kept as it is. (#123)
- Hire opens as a centred dialog, with a wider sign-in terminal and Open link and Copy link buttons. Notices float under the header instead of pushing the page down, and side panels open full screen on a phone. (#143)
- Plainer words for newcomers. Next leads through Hire, and Hire appears on its last step. A ticket that is still being worked on hides Accept and Ask for changes. Settings names each switch in plain words, and the signed-out page says how to sign in again. (#143, #144)

### Fixes

- If `dudes uninstall` stops partway, a rerun now removes the programs too, instead of reporting success while they are still installed. (#119)

### Breaking

- `dudes workspace` is removed, with no alias. Use `dudes project` (`add`, `remove`, `list`, `reload`, `check add` and `check remove`). Config keys are unchanged. (#120)
- `dudes doctor` is removed. Run `dudes status`, which prints the same report. `dudes floor open --no-browser` is now a usage error, so drop the flag. (#140)

## [0.91.0] - 2026-10-20

### Highlights

- **Give an agent a task from the first screen.** When an agent has nothing to do, the Floor shows "Give Kim a task" with one box and a Start button. Start creates the ticket and gives it to that agent in one step. With no agent yet, it says how to hire one, and with no coding app installed, it shows the install command. (#113)
- **See who needs you at a glance.** A strip of agent cards under the office puts agents whose work is ready for you first, then those that need you, each with one button to review, answer or chat. Tags over the agents in the office say the same, and on a phone a ticket opens full screen with Accept and Ask for changes always in reach. (#114)
- **Start in any repository.** Hiring from the Floor works where the project's tests already pass, and quick start in a repository with no test command offers a check that passes once the agent has changed a file, then you review the change. (#117)

### Changes

- Hire preselects an installed coding app and the only project, folds the role and look away, and keeps Hire on screen on a phone. A second agent in the same project gets a plain note on why its tickets wait for review, and how to avoid it. (#113)
- The Floor uses plain words: "Dudes ran it" for a check the daemon ran, "Checks passed today", and the journal position only behind Details. (#113, #116)
- `dudes quickstart --yes` turns on the Floor's hire, goal, control, answer and board actions when none are on yet. The terminal, renew, retire, changed-files and diff views stay off. (#115)

### Fixes

- The review sheet says in one sentence what Dudes could not check, the "is working on it" notice clears once a ticket is ready, and the sign-in and tab-limit messages say what to do. (#116)
- Hire counts no bytes, and the task card says plainly how Dudes checks the work. (#115)

## [0.90.0] - 2026-10-19

### Highlights

- **Agents end when the daemon crashes, on macOS and Linux.** A daemon that dies hard, from a crash, an out-of-memory kill or `kill -9`, now takes its agents with it within milliseconds, as it already did on Windows. Before, an agent kept working with nobody watching it. A restarted daemon ends that turn as vanished, and restart and continue resumes it. (#106, #107, #108)
- **The hire example gives a second agent a folder of its own.** The README and Get started no longer hire a second agent into the repository that quick start set up, so the first agent's tickets keep reaching Ready for you. (#109)

### Changes

- On macOS and Linux, the daemon starts a small keeper process beside each agent. The keeper ends the agent's process group when the daemon dies, however it dies. A restarted daemon no longer adopts an agent that outlived its daemon, because none does. A launch whose keeper cannot start fails before any task reaches the agent, and a daemon that cannot find its own executable refuses to start. (#106, #107)
- The README and Get started hire the example's second agent into `~/dudes-hello` and say why, and Get started names `--worktree` as the way to share one repository. (#109)

### Fixes

- On macOS, open an attached terminal under the same lock as every other child the daemon starts, so a terminal cannot inherit a keeper's pipe and stop its agent from ending with the daemon. Log a warning when a keeper ends on its own while its agent runs. (#108)

## [0.89.1] - 2026-10-19

### Highlights

- **Clearer first-run failures.** When an agent cannot launch because its engine is not installed, `dudes agents` and the Floor name the engine and the next step. When the daemon's socket path is too long for the platform, `dudes quickstart` and `dudes start` say so before anything starts. (#101, #103)
- **A Floor tab that is not signed in says so.** A refused, expired or missing sign-in link shows the problem and its remedy instead of an empty office, and the full page returns once a fresh link is pasted. (#102)

### Changes

- The README and Get started say to use `--provider codex` or `--provider pi` in the hire example when that is the CLI you have signed in, and the platforms page lists the Linux arm64 first-run rehearsal. (#100)

### Fixes

- Under a launch failure of kind NotFound, say that Dudes could not start the agent's engine, usually because its program is not installed, and give the next step, in `dudes agents` and in the Floor's drawer and crew row. (#101)
- Refuse `dudes quickstart` and `dudes start` before the daemon starts when its socket path is longer than the platform allows, naming the path, its length and the limit. Commands that connect to the daemon report the same limit. (#103)
- Show only the sign-in problem and its remedy on a Floor tab that holds no session, and show the full page as soon as a pasted link signs it in. (#102)
- When a check program is inside the project's repository, suggest running a shell script through its interpreter, for example `sh test.sh`, and say that the agent can still edit that script. A ticket's Needs you reason gives the same advice. (#104)

## [0.89.0] - 2026-10-18

### Highlights

- **Choose how an interrupted goal continues.** Inspect an eligible Managed goal in the CLI or authenticated Floor, choose an offered context, and authorize one recovery attempt. Recovery keeps the same agent and unfinished goal. Ordinary Resume does not release its recovery barrier. (#89)
- **Review recovered ticket attempts.** Recovery keeps the same ticket round. The Floor shows recovered attempts, earlier acknowledged reviews, and whether the last check applies to current work. An earlier check becomes Historical after later work is admitted. Unknown applicability is not a pass. (#89)

### Changes

- Offer **Try recorded conversation**, **Start fresh with recorded recap**, or **Repeat issued task in fresh context** when the inspected boundary permits them. Recorded context does not guarantee restored provider history. Fresh context does not restore history. A recorded recap may shrink or be omitted. Repeating an issued task preserves its task and answers without a recap. A context failure never automatically falls back to fresh work. Earlier effects may repeat. (#89)
- Keep recovery subject to budgets, hop caps, questions, instruction review, and other admission gates. Authorization does not guarantee a launch. Floor recovery requires both `floor.recovery` and `floor.control`. Quickstart's bundled Floor actions do not enable recovery. (#89)
- Keep the recovery request ID. If the reply is lost or authorization is unknown, read that request's receipt without resubmitting. Save the Floor's ID before reloading, then use `dudes recovery receipt <request-id>` afterward. (#89)
- After unused browser consent becomes invalidated, ordinary Resume and waiting do not reopen it. For a last Observed Failed or NoResult turn, a later explicit Pause can permit a fresh inspection and a new offered choice. See [the recovery steps](docs/goals-and-stopping.md#make-a-new-choice-after-unused-consent-is-invalidated). (#89)
- Use one Board with consistent ticket columns. Keep plain cards whose status does not match a ticket column under **Other board cards**. (#93)
- Add a **Workers** list to Overview for agents with their own worktrees, with their branch, state, and starting point. (#91)
- Link the running version's release notes from About and add a release-notes link to the Floor's post-update reload notice. (#90)
- New required recovery records and process-absence settlement data change the journal. Dudes 0.88.0 and older refuse journals containing this new data. Downgrading after it is written is unsupported. (#89)

### Fixes

- Explain an unknown checkout comparison when either the recorded review sample or the current sample is incomplete. (#88)
- Cap long agent names in office tags so they do not cover neighboring agents. (#92)
- Print the exact ticket Accept and Ask for changes commands, including the expected sequence, after the instruction comparison settles. (#94)
- Clear inherited blocked signals before starting agent, command-check, and terminal children on macOS and Linux, so they can receive graceful stop signals. (#95)

## [0.88.0] - 2026-10-16

### Highlights

- **Review ticket changes in the Floor.** For tickets in Ready for you or Needs you, the optional Changes view shows a tracked file's old and current text side by side. The comparison uses the commit recorded when the round started, so earlier uncommitted edits can appear. Enable `floor.git` and `floor.diff` with `dudes config set floor.git on floor.diff on`, then restart Dudes. Both switches are off by default. (#84)

### Changes

- Show bounded tracked text with reasons for omitted files. Count untracked files without showing their names or contents. The comparison requires a usable recorded baseline and is available on macOS and Linux. (#84)
- Show whether the current bounded sample is unchanged, changed, or unknown compared with the sample taken after the check. A match does not prove that the check passed on the displayed changes. Use Refresh to request a new sample. (#84)

## [0.87.2] - 2026-10-16

### Changes

- New ticket rounds record a checkout baseline before their first turn and a review stamp after completion checks. Once either new required record is written, Dudes 0.87.1 and older refuse to open that journal. (#78)

### Fixes

- Explain instruction-review gaps in plain language in the Floor and CLI, group repeated reasons, and distinguish standing engine limits from findings during the ticket. (#68)
- Explain both used or expired Floor sign-in links and the limit of four signed-in tabs, with steps to obtain a new link or end existing sign-ins. (#68)
- List each known changed instruction path once per context, even when it appears in multiple review intervals or is reported by more than one agent. (#72)
- Name the record kind when an instruction record is refused before append. Keep the actual journal position for failures in durable instruction history. (#80)
- Cancel ticket checkout captures when the daemon is shutting down. (#83)

## [0.87.1] - 2026-10-15

### Highlights

- **Passing tickets reach Ready for you.** In 0.87.0, the first ticket landed in Needs you for every engine because standing engine limits blocked instruction review. This release fixes that behavior. A ticket whose completion check passes and whose only instruction gaps are standing engine limits reaches Ready for you, with a note explaining what Dudes could not observe. Known shared instruction changes and gaps found during sampling still require review. (#65)
- **Keep Floor sign-ins across upgrades.** `dudes upgrade` carries live Floor sign-ins between versions that support the handover. An open tab asks for one reload after an upgrade or restart. Only an upgrade keeps the sign-in. After a plain restart, reloading asks you to sign in again. (#60, #61)

### Fixes

- Recognize native Linux ZFS and OverlayFS, including Docker filesystems, when sampling instruction files. Direct NFS, CIFS and SMB mounts remain refused. FUSE and unknown filesystem types also remain refused. OverlayFS may hide network-backed layers, whose locality and freshness Dudes does not establish. (#70)
- Quickstart prints the Floor link last and omits misleading failing-check advice for an idle first hire. Floor help explains that enabled actions let a session control agents. Uninstall names engine-history and snapshot folders only when they exist. (#69)
- Avoid a passing lone-agent ticket reaching Needs you because its finished child process was still counted as uncertain. Dudes now uses the host's proof that the child is gone when counting the folder's agents. (#67)
- Refuse an explicit completion check program path that resolves inside the project's repository when it is entered during quickstart, workspace enrollment, or check registration. Guided quickstart asks for another program before writing configuration. (#64)
- Warn that removing an agent worktree deletes ignored files, and show how to inspect the tree before removing it. (#62)

The running 0.87.0 daemon cannot hand over Floor sign-ins. After upgrading from 0.87.0, run `dudes floor open` to sign in again. A plain `dudes stop` followed by `dudes start` also ends Floor sessions.

## [0.87.0] - 2026-10-14

### Highlights

- **Review shared instruction changes.** Tickets require operator review when observed shared instructions change or their coverage is uncertain. The Floor and `dudes ticket show` display known changed paths, coverage gaps and observation details. Accept or Ask for changes applies to the exact reviewed state. (#57)
- **Upgrade and uninstall from the CLI.** `dudes upgrade` installs the latest release and restarts a running daemon; `--check` reports availability without installing. `dudes uninstall` previews removal and asks for confirmation. Both commands support macOS and Linux installer installations. Uninstall refuses to remove agent worktrees. (#55, #56)

### Changes

- Refuse inherited Claude context when a live peer shares the folder or owned-peer evidence is unresolved. Refusals explain when to use a separate worktree or wait for confirmed absence. (#57)
- When instruction review is pending, `dudes ticket accept` and `dudes ticket changes` require an explicit `--expected-seq` from `dudes ticket show`. (#57)
- Opt in to desktop notifications in Floor settings for tickets ready for review, tickets needing you and agent questions. Keep the Floor tab open; delivery depends on browser permission and the operating system.
- Open a ticket's recorded checkout with `dudes ticket open <card>`, using `VISUAL`, `EDITOR` or `--editor`. `--print` and `--json` inspect the directory without launching an editor. (#48)
- Document release checksum and provenance verification, plus Dudes and engine network/data boundaries. Add static regression checks for known outbound socket and DNS APIs. These checks do not sandbox child programs. (#50, #51)

### Fixes

- Refuse nonlocal Windows named-pipe overrides before attempting a control connection.
- Default repeated quickstart hiring to **No**, so pressing Enter does not hire another idle agent. (#47)

### Breaking

- Instruction observations and reviews add required journal records. Older readers refuse a journal containing these records; downgrading after they are written is unsupported. (#57)
- Rust ticket clients must include `instruction_review` in `TicketRow` and `CardTicket` constructors, and `review_watermark` in `TicketAssignParams` and `TicketAcceptParams`. (#57)
- Rust exhaustive matches must handle new variants in `Record`, `RecordKind`, `TicketReason`, `TicketRefusal`, `HoldReason`, supervisor `Effect` and `Seen`, and daemon `Answered` and `OnboardRefusal`. `OnboardRefusal` no longer implements `Copy`. (#57)

Instruction review compares bounded sampled endpoints. Windows and other unsupported platforms report coverage gaps. The comparison cannot identify an exclusive writer or prove that edits were not made and restored between samples.

## [0.86.0] - 2026-10-12

### Highlights

- **A board with room to work.** Board has its own working area, four counted workflow columns and a separate Needs you lane. All four columns fit at 1024 × 768, and Crew, office and terminal navigation keep their state when you leave Board. (#41, #44)
- **Review tickets in a side drawer.** Read the agent's summary, the daemon's check result and changed file names, then Accept or Ask for changes. New ticket opens a form with an optional agent; keyboard navigation and focus return keep the review flow usable. (#42)
- **See closing-time progress.** During `dudes stop`, the Floor shows acknowledged, settled, owed and running counts, plus time left. Disconnected views freeze their last observed values, and completion retains final counts. No Floor shutdown control or participant identities are added. (#45)

### Changes

- Give Board a full working area with a prominent New ticket action, counted workflow columns, empty-lane guidance and the newest 20 Done tickets. (#41)
- Add native ticket review and composer dialogs with plain-text content, state-appropriate actions and separated Stop/Remove confirmations. (#42)
- Add aggregate closing-time counts and a local countdown sampled from the daemon's existing deadline. Show early completion only when nothing remains owed or running and positive time remained; shutdown does not guarantee delivery of the final Complete frame to every subscriber. (#45)

### Fixes

- Keep ticket focus valid when a card changes or disappears, during nested Hire and after authority changes. Dismissed or replaced composers ignore late creation replies. (#42)
- Reduce workflow gaps in the existing tablet band so Done is fully visible at 1024 × 768, including the native vertical-scrollbar case. (#44)
- Publish closing-time updates in durable order and retain the original error when publication fails. (#45)

## [0.85.1] - 2026-10-12

### Highlights

- **Set up a project in one step.** Run `dudes workspace add` in a git repository to enroll it with its suggested completion check and sensible defaults. (#40)

### Changes

- Update TOML parsing and editing dependencies. (#37)
- `dudes workspace add` defaults to the canonical current directory, a unique folder id, the suggested check and its label, department `engineering`, five turns per goal and a 600-second check timeout. Explicit options override these defaults. (#40)
- Quickstart asks for a project folder outside a repository, explains when you skip it, and asks at most five questions including confirmation. Its first hire starts idle, and sign-in commands are printed. (#40)
- Without `--cwd`, `dudes hire` uses the only enrolled project or asks which one on a terminal. An agent hired without a project is named in a warning that explains it cannot take tickets. (#40)

### Fixes

- Start, Floor open, quickstart and ticket creation now explain when no project is set up and name `dudes workspace add` as the next step. The hint stays out of JSON output. The docs also explain how to bypass a shell alias or function named `dudes`. (#40)
- The Floor explains when tickets are off, when a project is missing, when no agent works in the selected project and when an agent hired without a project cannot take tickets. (#39)

## [0.85.0] - 2026-10-11

### Highlights

- **A ticket board.** Write a ticket, give it to an agent, and it starts at once. Each agent has a queue, and one ticket runs per project at a time. When the daemon's own run of the ticket's check passes, the ticket waits in Ready for you with the agent's summary, the changed files and the check result. You Accept it or Ask for changes. Tickets that can't go on wait in Needs you with a sentence that says what to do. Use it from the Floor's Board or with `dudes ticket`. (#19, #20, #21, #22, #23, #24, #25, #32, #33)
- **A quieter agent drawer.** The Output and Terminal tabs are gone: the full output opens from the agent's reply in Chat or from Overview, and Attach sits with the other actions. Open questions moved from the top bar to a "Waiting on you" section on Overview, which also lists tickets that need you. (#15, #16, #17)
- **Upgrades tell you to restart.** After an install, and in `dudes ping` and `dudes floor open`, Dudes says when the running daemon is still an older version and how to replace it. (#18)

### Changes

- New `dudes ticket` commands: `add`, `edit`, `give`, `show`, `list`, `accept`, `changes`, `stop` and `remove`. `--agent` takes an agent's name or id. (#21, #24)
- Giving a ticket starts the agent at once when it and the project are free, and queues it otherwise. A ticket sent back for changes goes ahead of new ones. Stop ends the agent's goal and puts the ticket back in To do; the agent's current step finishes first. (#22, #23)
- A ticket reaches Ready for you only after the daemon's own check passes following the agent's work. Only you move it to Done. Agents can't move ticket cards themselves. (#19, #20, #22)
- The Floor's Board shows To do, Working, Ready for you and Done, plus Needs you, with a ticket form, a panel for each ticket and a banner when one is ready or needs you. It is behind the new `floor.board` switch, which needs `floor.control` and `floor.goal`. `dudes quickstart`'s one Floor question now includes it and says that agents then run and spend under your account. To turn it on for an existing setup, run `dudes config set floor.board on` and restart the daemon. (#25, #32, #33)
- Once you create a ticket, the journal holds records that older versions of Dudes refuse to open, so downgrading after that is not possible. (#19)
- Releases are published only from commits on `main` whose CI passed. (#30)

### Fixes

- After an upgrade, a daemon left running on the old version is now named, instead of silently serving the old Floor. (#18)

## [0.84.0] - 2026-10-11

### Highlights

- **Quickstart asks less.** It turns on the engines it finds on your `PATH` and asks at most five questions: one consent question for Floor actions, the repository's completion check, and a first hire. It asks about a daily spending cap only when an API key is set. (#4)
- **The Floor link is printed, never opened.** `dudes floor open` and quickstart print a one-time link to paste into any browser on this computer. A link that was already used or has expired says so and tells you how to get a new one. (#5)
- **`dudes start` makes upgrading two commands.** It starts the daemon without any setup questions and warns when the running daemon is a different version from `dudes`. After installing a new release, run `dudes stop`, then `dudes start`. (#8)
- **A quieter Floor.** The spend totals card and the Traces tab are gone until there is a place for analytics, and the selected agent shows one status line and one row of actions. (#2, #3)

### Changes

- Quickstart detects engines instead of asking, asks one question for all Floor actions, and asks about a spending cap only when an API key is set. Pressing Enter at the Floor question turns Floor actions on; answer No to keep the Floor view-only. When no engine is on your `PATH`, the first-hire question defaults to No. (#4)
- `dudes floor open` and quickstart print the Floor link and never open a browser. `--no-browser` is still accepted and does nothing. A reused, expired or missing sign-in link gets its own message, and the docs explain how to reach the Floor over SSH. (#5)
- New `dudes start` command. It starts the daemon when it is not running, prints its version, socket and journal position, and names a version mismatch. Restart advice in the CLI and the docs now says `dudes stop`, then `dudes start`. (#8)
- The Floor no longer shows the spend totals card or the Traces tab. The daemon still records the same data. (#2)
- The selected agent's header in the Floor shows one status line and a compact row of actions. (#3)
- Releases carry structured notes built from this changelog, and each archive and `SHA256SUMS` carry a build provenance attestation you can check with `gh attestation verify`. (#7)

### Fixes

- The Floor's Git view no longer reports a large repository as unreadable at random on Linux. Git ran out of room to start its helper threads under the daemon's memory limit; the daemon's git reads now run without them. (#6)

## [0.79.0] - 2026-10-10

The first public release. Dudes is an early preview, so commands and configuration keys can still
change between releases. [Platforms and known limits](docs/platforms-and-limits.md) lists what has
been tested and what is missing.

### Added

- **A daemon and a CLI that supervise coding agents.** The `duded` daemon owns all state and records
  it in an append-only journal. The `dudes` command talks to it over a local socket. You hire
  Claude Code, Codex or Pi agents, each with a name, a role and a working directory.
- **Goals with completion checks.** `dudes goal` gives an agent a piece of work and a command. The
  daemon runs that command itself after each turn, and the goal is Done only when the daemon's run
  passes. A hop cap and optional spending ceilings stop work that does not converge.
- **The Floor.** A 3D office in the browser shows each agent's status, goal and open questions. You can
  answer, message, pause, retire or attach to an agent from it, through the Floor actions you turn
  on.
- **Mail and teams.** Agents send each other messages with `dudes mail` and find colleagues with
  `dudes team`.
- **Worktrees.** `dudes hire --worktree` gives a Claude Code agent its own git worktree and branch in
  a repository you mark as trusted.
- **Memory.** Agents keep notes across turns with `dudes memory append` and `dudes memory read`,
  and a long history is condensed.
- **MCP servers.** You declare MCP servers once and choose which agents get them.
- **Closing time.** `dudes stop` gives each agent a bounded chance to save its work before the daemon
  stops. `dudes stop --hard` stops at once.
- **Guided setup.** `dudes quickstart` turns on engines, starts the daemon, opens the Floor and can
  hire a first agent. `install.sh --from` installs a local build into `~/.local` without `sudo`.

[Unreleased]: https://github.com/ArcfluxResearch/dudes/compare/v0.100.0...HEAD
[0.100.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.99.1...v0.100.0
[0.99.1]: https://github.com/ArcfluxResearch/dudes/compare/v0.99.0...v0.99.1
[0.99.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.98.0...v0.99.0
[0.98.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.97.1...v0.98.0
[0.97.1]: https://github.com/ArcfluxResearch/dudes/compare/v0.97.0...v0.97.1
[0.97.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.96.0...v0.97.0
[0.96.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.95.0...v0.96.0
[0.95.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.94.0...v0.95.0
[0.94.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.93.0...v0.94.0
[0.93.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.92.0...v0.93.0
[0.92.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.91.0...v0.92.0
[0.91.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.90.0...v0.91.0
[0.90.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.89.1...v0.90.0
[0.89.1]: https://github.com/ArcfluxResearch/dudes/compare/v0.89.0...v0.89.1
[0.89.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.88.0...v0.89.0
[0.88.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.87.2...v0.88.0
[0.87.2]: https://github.com/ArcfluxResearch/dudes/compare/v0.87.1...v0.87.2
[0.87.1]: https://github.com/ArcfluxResearch/dudes/compare/v0.87.0...v0.87.1
[0.87.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.86.0...v0.87.0
[0.86.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.85.1...v0.86.0
[0.85.1]: https://github.com/ArcfluxResearch/dudes/compare/v0.85.0...v0.85.1
[0.85.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.84.0...v0.85.0
[0.84.0]: https://github.com/ArcfluxResearch/dudes/compare/v0.79.0...v0.84.0
[0.79.0]: https://github.com/ArcfluxResearch/dudes/releases/tag/v0.79.0
