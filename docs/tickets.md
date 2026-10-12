# Tickets

A ticket is a piece of work on the board. It has a title, an optional note, a project and a
"Done when" check. You make tickets, give them to agents, and review the agents' work with
`dudes ticket`, or on the Floor's Board once `floor.board` is on. See [The Floor](floor.md#tickets)
for the Floor's form, columns, ticket panel and banner. Both do the same things, and what one makes
the other shows.

Everything here needs a running daemon (`dudes start`) and an enrolled project, which is a
repository directory added with `dudes workspace add`. Each command answers at once and waits for
nothing.

To set up a project, run this from its git repository:

```sh
cd /absolute/path/to/repo
dudes workspace add
```

The command uses the repository's suggested completion check. If it finds none, pass one with
`dudes workspace add -- <program> [args...]`. It prints the enrolled project id.
Hire the agent into that project. Without `--cwd`, `dudes hire` uses the only project or asks which
one on a terminal. An agent hired without a project cannot take tickets.

## Make a ticket and give it to an agent

```sh
dudes ticket add --title "Fix the sign-in page" --note "It returns a 500 on a blank password."
```

The command prints `ticket <card id>`. The card id is the number every other `dudes ticket` command
takes. Add `--json` for the same answer as data.

- **Project.** `--project <id>` names the enrolled project. When only one project is enrolled, it
  is the default.
- **Done when.** `--done-when <check id>` picks one of the project's named checks. The default is
  the project's own check. The daemon runs it, and what an agent says about its own work never
  counts as passing.
- **Note.** Optional. A one-line ticket is a normal ticket.

Give the ticket to an agent hired into the same project:

```sh
dudes ticket give <card id> --agent Cody
```

`--agent` takes the agent's name or its id. A name must match exactly one live agent. When no live
agent has that name, or several do, the command prints a plain sentence (the second lists the
matching ids) and sends nothing, and you can use an id instead.

The answer is immediate and says what happened:

- `Cody is working on it.` The agent was idle and its project was free, so the ticket started.
- `Queued for Cody, 2nd. Waiting for ...` The ticket is in that agent's queue, which holds up to
  eight tickets, and the sentence says what it waits for: the agent's current work, or the ticket
  that holds the project. A project runs one ticket at a time, and a ticket that is Ready for you
  holds it until it is accepted or sent back.
- `Queued. Cody is paused.` A paused or halted agent starts nothing until it is resumed.
- `Needs you: ...` The ticket could not start for a reason only you can fix, and the sentence
  says what to do.

`--message "<text>"` adds a note for the agent. To make and give a ticket in one step, pass
`--give-to <name or id>` to `dudes ticket add`. If the give is refused, the ticket still exists and
the refusal is printed.

## Review the work

When the agent finishes a turn, the daemon runs the ticket's check itself. If it passes, the
ticket moves to Ready for you. What the agent says about its own work never counts.

```sh
dudes ticket show <card id>
```

For a ticket that has been worked on, `show` prints "The agent says:" followed by the agent's last
reply, and a line such as `Check: passed, exit status 0, 2026-10-11 09:14:02 +00:00`. A check
that never ran or was killed prints `no exit status`. The changed files are not listed in the
terminal. Open the Floor to see them.

Then decide:

```sh
dudes ticket accept <card id>
dudes ticket changes <card id> --message "Also cover the empty form."
```

- `accept` marks a Ready for you ticket Done. Accepting is your verdict that the work is what you
  asked for. Passing the check only says the check passed.
- `changes` sends the ticket back with what to change. It goes to the agent that did the work, or
  to the one named with `--agent <name or id>`, and goes to the front of that agent's queue. The
  message is required. It works on a ticket that is Ready for you or Needs you, and is the same
  as `give --message`. A ticket in Needs you can also be retried with `give` and no message.

## Take a ticket back

```sh
dudes ticket stop <card id>
```

`stop` takes a Working or Needs you ticket back to To do and gives the agent back, so it can take
the next ticket. It issues no new turn. A turn that is already running is not killed and ends on
its own, and the project stays busy until then. Whatever the agent already wrote stays in the
project folder, because Dudes never reverts, merges or pushes.

A whole loop looks like this:

```sh
dudes ticket add --title "Fix the sign-in page" --note "It 500s on a blank password."
dudes ticket give <card id> --agent Cody
dudes ticket show <card id>
dudes ticket accept <card id>          # or: dudes ticket changes <card id> --message "..."
dudes ticket stop <card id>            # only for a ticket that is still being worked on
```

## Look at tickets

```sh
dudes ticket list
dudes ticket list --status todo
dudes ticket show <card id>
```

`--status` takes `todo`, `working`, `ready`, `done` or `needs-you`. `show` prints the note, the
check, the agent, the agent's summary and the check result.

## Fix a mistake

```sh
dudes ticket edit <card id> --title "Fix the sign-in page for good"
dudes ticket remove <card id>
```

Edit changes the title, the note (`--note ""` clears it) or the check. Edit works on a ticket
nobody has started, and remove on any ticket that is not being worked on; stop a Working ticket
first. Each command reads the ticket's current sequence just before it acts. Pass
`--expected-seq <n>` only when a script wants a ticket that has changed since it looked to be
refused.

## When a command is refused

A refusal prints one plain sentence on the error stream. Exit status 3 means the daemon declined,
for example because the card is not a ticket, the ticket is in a state the command does not
apply to ("You can't do that to a ticket that is To do."), the project is not enrolled, or the
agent is not available for the project. Exit status 4 means the ticket changed since the sequence
given, an agent name that matches no live agent or several, or another error. `dudes --help` lists every exit status.
