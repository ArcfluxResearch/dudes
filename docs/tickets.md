# Tickets

A ticket is a piece of work on the board. It has a title, an optional note, a project and a
"Done when" check. You make tickets and give them to agents with `dudes ticket`.

Everything here needs a running daemon (`dudes start`) and an enrolled project, which is a
repository directory added with `dudes workspace add`. Each command answers at once and waits for
nothing.

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
dudes ticket give <card id> --agent <agent id>
```

The answer is immediate: `Queued for Cody, 1st.` The ticket waits in that agent's queue, which
holds up to eight tickets. `--message "<text>"` adds a note for the agent. To make and give a
ticket in one step, pass `--give-to <agent id>` to `dudes ticket add`. If the give is refused, the
ticket still exists and the refusal is printed.

## Look at tickets

```sh
dudes ticket list
dudes ticket list --status todo
dudes ticket show <card id>
```

`--status` takes `todo`, `working`, `ready`, `done` or `needs-you`. `show` prints the note, the
check and the agent.

## Fix a mistake

```sh
dudes ticket edit <card id> --title "Fix the sign-in page for good"
dudes ticket remove <card id>
```

Edit changes the title, the note (`--note ""` clears it) or the check. Both work on a ticket
nobody has started. Each command reads the ticket's current sequence just before it acts. Pass
`--expected-seq <n>` only when a script wants a ticket that has changed since it looked to be
refused.

## When a command is refused

A refusal prints one plain sentence on the error stream. Exit status 3 means the daemon declined,
for example because the card is not a ticket, the project is not enrolled, or the agent is not
available for the project. Exit status 4 means the ticket changed since the sequence given, or
another error. `dudes --help` lists every exit status.
