# Shared harness for the gate arms. Sourced, never executed.
#
# Everything here is plumbing: starting and stopping a real daemon, driving the real CLI, counting
# real processes, and reading the journal back with the shipped reader. **Nothing here decides
# anything the daemon decides.** An acceptance harness that carried its own copy of the loop would
# keep passing after the daemon drifted away from it, and that is the one failure this kind of
# script must not have.
#
# Conventions, one per check:
#   header <text>   one proposition, announced before it is tested
#   pass / fail     one line each, and `fail` counts
#   finish          exits 1 if anything failed, 0 otherwise
set -u

# --- where things are ---------------------------------------------------------------------------

EXAMPLE_DIR=$(cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(cd "$EXAMPLE_DIR/../.." && pwd)
TARGET_DIR=${CARGO_TARGET_DIR:-$REPO_ROOT/target}
# Kept artefacts: the journal dumps and projection exports each arm produced. Under the target
# directory so they are ignored by git, and at a fixed path so the control arm can compare its run
# with the crashed one (the control is a comparison *between* two runs).
GATE_OUT=${DUDES_GATE_OUT:-$TARGET_DIR/two-roles-evidence}
# How old the crashed run's artefacts may be when the control arm compares against them. The three
# scripts are meant to be run one after another, so anything approaching an hour is a leftover from
# an earlier session rather than this attempt's — see `compare_with_the_crashed_run`.
MAX_ATTEMPT_AGE=${DUDES_GATE_MAX_AGE:-3600}

DUDED=${DUDED:-$TARGET_DIR/release/duded}
DUDES_CLI=${DUDES_CLI:-$TARGET_DIR/release/dudes}

failures=0
daemon_pid=""

header() { printf '\n--- %s\n' "$1"; }
pass() { printf 'PASS  %s\n' "$1"; }
fail() {
	printf 'FAIL  %s\n' "$1"
	failures=$((failures + 1))
}
note() { printf '      %s\n' "$1"; }

# A setup problem, not a proposition: there is nothing to test if this fails.
fatal() {
	printf 'ERROR %s\n' "$1" >&2
	exit 1
}

finish() {
	if [ "$failures" -eq 0 ]; then
		printf '\nPASS  %s\n' "$1"
		exit 0
	fi
	printf '\nFAIL  %s: %s check(s) failed\n' "$1" "$failures"
	exit 1
}

require_binaries() {
	for binary in "$DUDED" "$DUDES_CLI"; do
		[ -x "$binary" ] || fatal "no $binary — run: cargo build --release --workspace"
	done
}

# --- the sandbox ---------------------------------------------------------------------------------

# A directory of this run's own, removed on the way out along with any daemon still running.
open_sandbox() {
	SANDBOX=$(mktemp -d "${TMPDIR:-/tmp}/dudes-two-roles-XXXXXX") || fatal "could not make a sandbox"
	STATE_DIR="$SANDBOX/state"
	RUNTIME_DIR="$SANDBOX/runtime"
	WORK_DIR="$SANDBOX/work"
	HOME_DIR="$SANDBOX/home"
	# One directory that is `$HOME/Library/Application Support` on macOS, `$XDG_CONFIG_HOME` on
	# Linux and `%APPDATA%` on Windows, so the daemon finds `dudes/config.toml` under it on each.
	CONFIG_ROOT="$HOME_DIR/Library/Application Support"
	mkdir -p "$STATE_DIR" "$RUNTIME_DIR" "$WORK_DIR" "$GATE_OUT" "$CONFIG_ROOT/dudes"
	# The daemon refuses to bind inside a runtime directory anyone else can write: the operator
	# credential is minted in there. A directory this script creates gets the umask's mode, so it
	# is set here rather than left to whatever the machine's umask happens to be.
	chmod 700 "$RUNTIME_DIR"
	# The arms hire with a dollar ceiling, and the daemon refuses that hire until the operator file
	# sets a positive per-turn reserve. The sandbox brings its own file, so an arm runs the same on
	# a machine with no operator file and is not changed by the operator's own.
	printf '[budget.per_turn_floor]\nusd = 0.01\n' >"$CONFIG_ROOT/dudes/config.toml"
	trap close_sandbox EXIT INT TERM
}

close_sandbox() {
	if [ -n "$daemon_pid" ] && kill -0 "$daemon_pid" 2>/dev/null; then
		kill -9 "$daemon_pid" 2>/dev/null
	fi
	# Only ever the directory this script created (`mktemp -d`), never a path handed in.
	[ -n "${SANDBOX:-}" ] && rm -rf "$SANDBOX"
}

# --- the daemon -----------------------------------------------------------------------------------

# start_daemon <log-suffix> [env assignments...]
#
# The stand-in agent's directory goes on PATH: the daemon resolves a managed turn's program by
# name, so this is what makes the loop close with no vendor account. Everything else the daemon
# needs it takes from its own two directory variables and the sandbox's operator file.
start_daemon() {
	suffix=$1
	shift
	log="$SANDBOX/duded.$suffix.log"
	PATH="$EXAMPLE_DIR/bin:$PATH" \
		HOME="$HOME_DIR" \
		XDG_CONFIG_HOME="$CONFIG_ROOT" \
		APPDATA="$CONFIG_ROOT" \
		DUDES_STATE_DIR="$STATE_DIR" \
		DUDES_RUNTIME_DIR="$RUNTIME_DIR" \
		DUDES_EXAMPLE_WORK="$WORK_DIR" \
		DUDES_EXAMPLE_MARK_ON_RUN="${MARK_ON_RUN:-2}" \
		env "$@" "$DUDED" >"$log" 2>&1 &
	daemon_pid=$!
	await "the daemon answers system.ping" 30 dudes_ping || {
		note "daemon log:"
		sed 's/^/      | /' "$log" >&2
		fatal "the daemon never came up (log above)"
	}
}

dudes_ping() { cli ping >/dev/null 2>&1; }

# The operator's door. `DUDES_RUNTIME_DIR` is where the daemon put the socket and the credential.
cli() {
	DUDES_RUNTIME_DIR="$RUNTIME_DIR" "$DUDES_CLI" "$@"
}

stop_daemon() {
	[ -n "$daemon_pid" ] || return 0
	kill -TERM "$daemon_pid" 2>/dev/null
	waited=0
	while kill -0 "$daemon_pid" 2>/dev/null; do
		waited=$((waited + 1))
		[ "$waited" -gt 300 ] && fatal "the daemon did not stop within 30s"
		sleep 0.1
	done
	wait "$daemon_pid" 2>/dev/null
	daemon_pid=""
}

kill_daemon_hard() {
	kill -9 "$daemon_pid" 2>/dev/null
	waited=0
	while kill -0 "$daemon_pid" 2>/dev/null; do
		waited=$((waited + 1))
		[ "$waited" -gt 300 ] && fatal "the daemon survived SIGKILL for 30s"
		sleep 0.1
	done
	wait "$daemon_pid" 2>/dev/null
	daemon_pid=""
}

# --- counting children -----------------------------------------------------------------------------

# Direct children of the running daemon, right now.
#
# The daemon creates two classes of child: an agent's managed turn, and a goal predicate. Both are
# direct children, so this counts both — which is exactly why every arm counts at a moment when the
# turn's child is *held* and no predicate can be running: a predicate runs only after a turn has
# ended. That is a property of the loop the harness waits for, not a filter it applies.
#
# # What this cannot see, and the assertion that makes that safe
#
# It filters on the **current** daemon's pid, and `start_daemon` re-reads that pid on a restart. A
# child that outlived a `SIGKILL` is re-parented away from the dead pid and is not a child of the
# new one, so **an orphan is invisible here** — and an orphan beside a re-issued turn is precisely
# the "not two" that arm 2 exists to exclude. Nothing in a parent-pid walk can close that; it is the
# same hole the Windows side has, for the same reason.
#
# What closes it is elsewhere and is load-bearing: `crash.sh`'s assertion that **zero** children are
# live in the stall window, taken immediately before the kill. Nothing can be orphaned by a kill
# that lands when nothing is running. Move that assertion, weaken it, or arm the fault at a launch
# for which it does not hold, and this count stops being exact — the arm goes quiet rather than red.
# Treat the two as one mechanism.
#
# On Windows, the same direct-parent query is made outside the daemon through CIM. It only runs
# while `daemon_pid` names a live daemon. The parent row has to exist, and a child whose creation
# date predates its purported parent is discarded: Windows documents that a ParentProcessId can
# refer to a reused identifier. This is an observation of the process table, not an answer supplied
# by the daemon under test.
#
# This has the orphan hole described above. Arm 2 closes that hole by proving the stall window is
# empty before it kills the daemon. It does not make a general orphan census available.
windows_live_child_pids() {
	case "$daemon_pid" in
	"" | *[!0-9]*)
		printf '%s\n' "Windows child count needs the running daemon's numeric pid" >&2
		return 1
		;;
	esac

	# `-Command <string>` has no portable trailing-argument boundary. Pass the already validated
	# decimal pid in an invocation-scoped environment variable instead: it is data read through
	# `$env:`, never text appended to PowerShell source. The receipt is a separate file because this
	# function's stdout is reserved for PID records consumed by `capture_live_child_snapshot`.
	parent_receipt="$SANDBOX/cim-parent-pid"
	child_pids="$SANDBOX/cim-child-pids"
	rm -f "$parent_receipt" "$child_pids"
	if ! DUDES_GATE_CIM_PARENT_PID="$daemon_pid" \
		DUDES_GATE_CIM_PARENT_PID_RECEIPT="$parent_receipt" \
		powershell.exe -NoLogo -NoProfile -NonInteractive -Command '
		$ErrorActionPreference = "Stop"
		$parentText = $env:DUDES_GATE_CIM_PARENT_PID
		if ($parentText -notmatch "^[0-9]+$") { throw "missing numeric parent pid" }
		if ([string]::IsNullOrEmpty($env:DUDES_GATE_CIM_PARENT_PID_RECEIPT)) {
			throw "missing parent pid receipt path"
		}
		Set-Content -LiteralPath $env:DUDES_GATE_CIM_PARENT_PID_RECEIPT -Value $parentText -NoNewline -Encoding ascii
		$parentId = [uint32]$parentText
		$parents = @(Get-CimInstance -ClassName Win32_Process -Filter "ProcessId = $parentId")
		if ($parents.Count -ne 1) { throw "no unique live parent for pid $parentId" }
		$parent = $parents[0]
		if ($null -eq $parent.CreationDate) { throw "parent pid $parentId has no CreationDate" }
		Get-CimInstance -ClassName Win32_Process -Filter "ParentProcessId = $parentId" |
			Where-Object { $null -ne $_.CreationDate -and $_.CreationDate -ge $parent.CreationDate } |
			ForEach-Object { [string]$_.ProcessId }
	' >"$child_pids"; then
		printf '%s\n' "Windows CIM child collection failed for daemon pid $daemon_pid" >&2
		rm -f "$parent_receipt" "$child_pids"
		return 1
	fi
	if ! received_parent=$(cat "$parent_receipt") || [ "$received_parent" != "$daemon_pid" ]; then
		printf '%s\n' "Windows CIM did not receive the daemon pid it was given" >&2
		rm -f "$parent_receipt" "$child_pids"
		return 1
	fi
	# The read's status is the collector's status. `cat` straight to stdout would leave the function
	# to be judged by the `rm` that follows it, and a cleanup that succeeds after a read that failed
	# presents an empty stream as a *successful* collection — a count of zero the caller cannot tell
	# from a live daemon with no children, which is the one answer the zero-control exists to
	# reject. So the records are read into a variable first, the sandbox files go on both branches,
	# and an unreadable file is a refusal.
	if ! collected_pids=$(cat "$child_pids"); then
		printf '%s\n' "Windows CIM child records could not be read for daemon pid $daemon_pid" >&2
		rm -f "$parent_receipt" "$child_pids"
		return 1
	fi
	rm -f "$parent_receipt" "$child_pids"
	# An empty set is a real answer here — a live daemon between turns has no children — so it is
	# returned as one, explicitly, rather than left to whatever `printf` of an empty string means.
	[ -n "$collected_pids" ] || return 0
	printf '%s\n' "$collected_pids"
}

live_child_pids() {
	case "$(uname -s)" in
	MINGW* | MSYS* | CYGWIN* | Windows_NT)
		windows_live_child_pids
		;;
	*)
		ps -A -o ppid=,pid= | awk -v parent="$daemon_pid" '$1 == parent { print $2 }'
	esac
}

# Capture one ordered PID set for an assertion. A collection failure stays a failure; it must not
# become an empty set through a pipeline's `|| true` and pass the zero-control by accident.
capture_live_child_snapshot() {
	if ! LIVE_CHILD_PIDS=$(live_child_pids); then
		return 1
	fi
	# Windows PowerShell writes CRLF even when invoked by Git Bash. Remove only that record separator
	# before validating the decimal PID records; a bare `awk` check would otherwise reject a sound
	# Windows snapshot as malformed.
	LIVE_CHILD_PIDS=$(printf '%s\n' "$LIVE_CHILD_PIDS" | tr -d '\r')
	if [ -n "$LIVE_CHILD_PIDS" ] && ! printf '%s\n' "$LIVE_CHILD_PIDS" | awk '
		/^[0-9][0-9]*$/ { next }
		{ exit 1 }
	'; then
		printf '%s\n' "child collection returned a non-numeric process identifier" >&2
		return 1
	fi
	LIVE_CHILD_PIDS=$(printf '%s\n' "$LIVE_CHILD_PIDS" | awk 'NF' | sort -n -u)
	LIVE_CHILD_COUNT=$(printf '%s\n' "$LIVE_CHILD_PIDS" | awk 'NF { count += 1 } END { print count + 0 }')
}

live_children() {
	capture_live_child_snapshot || return 1
	printf '%s\n' "$LIVE_CHILD_COUNT"
}

# --- the Windows collector's own controls --------------------------------------------------------
#
# `windows_live_child_pids` has two ways to be wrong that no POSIX run of any arm reaches, and that
# a native Windows run reaches only when something is already going wrong: a receipt naming a pid
# other than the one the collector was handed, and child records that cannot be read after the
# PowerShell step has already succeeded. The second is the dangerous one, because its failure mode
# is silence — an unread file handed to `capture_live_child_snapshot` is an *empty set*, and an
# empty set is a count of zero, which is precisely the answer the zero-control exists to reject.
#
# So both branches are exercised directly, with a stand-in for the PowerShell step: a synthetic
# parent pid, and a `powershell.exe` on PATH that writes the receipt and the PID records the probe
# asks it for. No daemon, no release binary and no CIM query is involved — what is under test is the
# shell's handling of that step's results, which is the same code on every host. The first two
# propositions are the positive controls: they are what say the stand-in reaches a *successful*
# collection, with and without records, and without them the refusals below would be a probe that
# never worked refusing itself.
COLLECTOR_PROBE_PID=4242

# collector_probe <receipt-pid> <records> <remove-child-records yes|no>
#
# One call of the collector against the stand-in, which writes `<receipt-pid>` to the receipt and
# prints `<records>` (a space-separated list, empty for a daemon with no children). Sets
# `probe_status`, `probe_pids` (its stdout), `probe_error` (its stderr) and `probe_leftovers` (any
# sandbox file it failed to clean up). The call runs inside a command substitution, so the
# `daemon_pid` and `PATH` it needs are the subshell's and never the caller's — `close_sandbox` must
# not be left holding a synthetic pid.
collector_probe() {
	probe_error="$COLLECTOR_STUB_DIR/probe.err"
	probe_pids=$(
		daemon_pid=$COLLECTOR_PROBE_PID
		PATH="$COLLECTOR_STUB_DIR:$PATH"
		DUDES_GATE_STUB_RECEIPT_PID="$1" \
			DUDES_GATE_STUB_RECORDS="$2" \
			DUDES_GATE_STUB_REMOVE="$3" \
			DUDES_GATE_STUB_CHILD_PIDS="$SANDBOX/cim-child-pids" \
			windows_live_child_pids 2>"$probe_error"
	)
	probe_status=$?
	probe_leftovers=""
	for leftover in "$SANDBOX/cim-parent-pid" "$SANDBOX/cim-child-pids"; do
		[ -e "$leftover" ] && probe_leftovers="$probe_leftovers ${leftover##*/}"
	done
	rm -f "$SANDBOX/cim-parent-pid" "$SANDBOX/cim-child-pids"
}

check_windows_collector_failure_branches() {
	COLLECTOR_STUB_DIR="$SANDBOX/collector-control"
	mkdir -p "$COLLECTOR_STUB_DIR" || fatal "could not make the collector control's directory"
	cat >"$COLLECTOR_STUB_DIR/powershell.exe" <<'STUB'
#!/bin/sh
# Stand-in for the collector's PowerShell step. Not an emulation of Windows: it does what that step
# does for the shell around it — write the receipt, print PID records — and lets the probe choose
# which of those to get wrong.
set -u
printf '%s' "$DUDES_GATE_STUB_RECEIPT_PID" >"$DUDES_GATE_CIM_PARENT_PID_RECEIPT"
if [ -n "$DUDES_GATE_STUB_RECORDS" ]; then
	# Unquoted on purpose: the probe hands over a space-separated list of PID records, and the
	# empty list is a daemon with no children, which prints nothing rather than a blank line.
	printf '%s\n' $DUDES_GATE_STUB_RECORDS
fi
if [ "$DUDES_GATE_STUB_REMOVE" = yes ]; then
	# The collector's child-records path, named rather than discovered: a stand-in cannot see the
	# redirection its caller set up. If the collector ever renames that file this removes nothing,
	# the read succeeds, and the probe that requires a refusal goes red — the coupling fails loudly
	# rather than quietly passing.
	rm -f "$DUDES_GATE_STUB_CHILD_PIDS"
fi
exit 0
STUB
	chmod +x "$COLLECTOR_STUB_DIR/powershell.exe" || fatal "could not make the stand-in executable"

	header "the collector returns the PID records its PowerShell step produced"
	collector_probe "$COLLECTOR_PROBE_PID" "4243 4244" no
	if [ "$probe_status" -eq 0 ] && [ "$probe_pids" = "$(printf '%s\n' 4243 4244)" ] &&
		[ -z "$probe_leftovers" ]; then
		pass "status 0, the records 4243 and 4244, and no file left in the sandbox"
	else
		fail "wanted 4243 and 4244 collected; status $probe_status, records $(printf '%s' "$probe_pids" | tr '\n' ' ')"
		note "leftovers:${probe_leftovers:- none}"
		note "stderr: $(head -n 1 "$probe_error")"
	fi

	# The counterpart of the refusal below, and not a formality: arm 2's pre-kill window asserts a
	# count of *zero*, so a collector that answered an empty record set with a refusal would fail
	# the arm on Windows at the one moment it is meant to pass.
	header "a step that produced no records is an empty collection, not a refusal"
	collector_probe "$COLLECTOR_PROBE_PID" "" no
	if [ "$probe_status" -eq 0 ] && [ -z "$probe_pids" ] && [ -z "$probe_leftovers" ]; then
		pass "status 0 and no records: a live daemon between turns"
	else
		fail "wanted an empty collection; status $probe_status, records $(printf '%s' "$probe_pids" | tr '\n' ' ')"
		note "stderr: $(head -n 1 "$probe_error")"
	fi

	header "a receipt naming another pid is refused"
	collector_probe 9999 "4243 4244" no
	if [ "$probe_status" -ne 0 ] && grep -q 'did not receive the daemon pid' "$probe_error" &&
		[ -z "$probe_leftovers" ]; then
		pass "the collector refused a receipt of 9999 for pid $COLLECTOR_PROBE_PID"
	else
		fail "wanted a refusal naming the receipt; status $probe_status"
		note "stderr: $(head -n 1 "$probe_error")"
	fi

	header "child records that cannot be read are a refusal, never an empty collection"
	collector_probe "$COLLECTOR_PROBE_PID" "4243 4244" yes
	if [ "$probe_status" -ne 0 ] && grep -q 'could not be read' "$probe_error" &&
		[ -z "$probe_pids" ] && [ -z "$probe_leftovers" ]; then
		pass "the read failed after a successful step, and the collector returned $probe_status"
	else
		fail "a failed child-records read was reported as status $probe_status with $(printf '%s' "$probe_pids" | tr '\n' ' ' | wc -w | tr -d ' ') record(s)"
		note "stderr: $(head -n 1 "$probe_error")"
		note "leftovers:${probe_leftovers:- none}"
	fi

	# The proposition the arm actually rests on: that refusal reaches the snapshot as a failure and
	# not as a count. `live_child_pids` is named here because the host running this control is not
	# Windows and `uname` would route to `ps`.
	header "a refused collection sets no count for the exact-one assertion to read"
	snapshot=$(
		daemon_pid=$COLLECTOR_PROBE_PID
		PATH="$COLLECTOR_STUB_DIR:$PATH"
		live_child_pids() { windows_live_child_pids; }
		DUDES_GATE_STUB_RECEIPT_PID=$COLLECTOR_PROBE_PID \
			DUDES_GATE_STUB_RECORDS="4243 4244" \
			DUDES_GATE_STUB_REMOVE=yes \
			DUDES_GATE_STUB_CHILD_PIDS="$SANDBOX/cim-child-pids" \
			capture_live_child_snapshot 2>/dev/null
		printf 'status=%s count=%s\n' "$?" "${LIVE_CHILD_COUNT:-unset}"
	)
	rm -f "$SANDBOX/cim-parent-pid" "$SANDBOX/cim-child-pids"
	case $snapshot in
	"status=0"* | *"count=0") fail "the snapshot took a failed read as a result: $snapshot" ;;
	*"count=unset") pass "the snapshot failed and left no count: $snapshot" ;;
	*) fail "unexpected snapshot outcome: $snapshot" ;;
	esac
}

# The pid the stand-in wrote for run $1, so an arm can say *which* process it counted rather than
# only how many.
standin_pid() { cat "$WORK_DIR/started.$1" 2>/dev/null; }

# Whether a process is gone. `kill -0` and not `wait`, because the one process an arm asks this
# about is a child the kill orphaned: the shell can wait only on its own children, and that one is
# nobody's.
process_gone() { ! kill -0 "$1" 2>/dev/null; }

# --- the stand-in's session transcripts ----------------------------------------------------------
#
# `bin/claude` keeps one file per session id under the work directory and appends to it, which is
# the shape a coding-agent CLI's session files have: two turns on one session share a file, and two
# sessions never share one. So *which children wrote into which session* is answerable from the
# file system — and the file system is the only place it is answerable at all. These are the
# vendor's files; the daemon does not own them, and the one belonging to a child that crashed
# before its `TurnSpawned` is named by no record anywhere.

# Every session that has a transcript, one id per line, sorted.
transcript_sessions() {
	[ -d "$WORK_DIR/transcripts" ] || return 0
	for file in "$WORK_DIR"/transcripts/*.jsonl; do
		[ -e "$file" ] || continue
		name=${file##*/}
		printf '%s\n' "${name%.jsonl}"
	done | sort
}

# The distinct pids that wrote into session $1, one per line, sorted. A *set*, so a child that
# wrote twice counts once and "did two children write here" is a line count.
pids_in_transcript() {
	grep -o '"pid":[0-9]*' "$WORK_DIR/transcripts/$1.jsonl" 2>/dev/null |
		cut -d: -f2 | sort -u
}

# Exactly one live child of the daemon, and it is the agent's turn — not merely something.
#
# The identity half is not decoration. A count on its own passes against a daemon that left some
# other process behind, and "exactly one" would then be a statement about arithmetic rather than
# about the turn. The stand-in writes its own pid, so the two sides of this come from different
# places: one from the process table, one from the child itself.
assert_one_child_is_the_agents() {
	run=$1
	want=$(standin_pid "$run")
	if ! capture_live_child_snapshot; then
		fail "could not collect the daemon's live children"
		return
	fi
	pids=$(printf '%s\n' "$LIVE_CHILD_PIDS" | tr '\n' ' ')
	if [ "$LIVE_CHILD_COUNT" -eq 1 ] && [ "$LIVE_CHILD_PIDS" = "$want" ]; then
		pass "exactly one live child of the daemon, and it is the agent's turn: pid $want"
	else
		fail "wanted one child, the stand-in's pid ${want:-unknown}; the daemon's are: ${pids:-none}"
	fi
}

release_standin() { : >"$WORK_DIR/release.$1"; }

# --- waiting ------------------------------------------------------------------------------------

# await <what> <seconds> <command...> — poll until the command succeeds, or report and return 1.
await() {
	what=$1
	seconds=$2
	shift 2
	waited=0
	limit=$((seconds * 10))
	while ! "$@"; do
		waited=$((waited + 1))
		if [ "$waited" -gt "$limit" ]; then
			note "gave up waiting for $what after ${seconds}s"
			return 1
		fi
		sleep 0.1
	done
	return 0
}

file_exists() { [ -e "$1" ]; }

# Whether the running daemon's journal already carries a record matching $1.
#
# Through `dudes tail`, which is the operator's route to the log: the journal is the authority, so a client must not read
# `state.redb` directly, and redb would refuse the open anyway while the daemon holds it. The
# offline reader is for after the daemon has stopped.
journal_shows() { cli --json tail --from 0 2>/dev/null | grep -qF "$1"; }

# --- reading the journal ---------------------------------------------------------------------------

# The journal as text, one line per record: `<lsn> <kind> <record as JSON>`. Read offline with the
# shipped reader, which is the only route that works after the crash arm's SIGKILL — there is no
# daemon left to ask, and redb refuses a second open of a store a daemon still holds.
dump_journal() {
	(cd "$REPO_ROOT" && cargo run --quiet --release -p xtask -- journal --state "$STATE_DIR") \
		2>"$SANDBOX/journal.err"
}

# The projection export: from the store a daemon left, or from a from-scratch replay of the
# journal beside it. `dump_export_of <state-dir> [journal]`; `dump_export` is this run's own.
dump_export_of() {
	if [ "${2:-store}" = "journal" ]; then
		(cd "$REPO_ROOT" && cargo run --quiet --release -p xtask -- \
			export --state "$1" --from-journal) 2>"$SANDBOX/export.err"
	else
		(cd "$REPO_ROOT" && cargo run --quiet --release -p xtask -- \
			export --state "$1") 2>"$SANDBOX/export.err"
	fi
}

dump_export() { dump_export_of "$STATE_DIR" "${1:-store}"; }

# Lines of a given record kind, from a dump.
records_of() { awk -v kind="$2" '$2 == kind' "$1"; }

# --- reading the shipped record contract -----------------------------------------------------------
#
# `schemas/journal-record.schema.json` is generated from the record type by `cargo xtask schemas`
# and held to it by `cargo xtask schemas --check`, so it is the nearest thing to the writer's own
# spelling that a shell script can read. An arm that searches the journal for a record kind or a
# field uses it as the control on its own spelling: a name spelled the way nothing writes it matches
# nothing, and an absence check whose search cannot match reports the absence it went looking for.
# Named and not globbed — `schemas/rpc.schema.json` carries the same literals, and a control that
# would pass on either file is a control on the wrong corpus.
RECORD_CONTRACT="$REPO_ROOT/schemas/journal-record.schema.json"

# Whether the contract defines a record kind $1 — and, when $2 is given, one that declares a field
# named $2 **on that record**.
#
# The kind is matched on its own `"t"` const and the field inside the same union member, so this
# answers a question about one record rather than "does the file contain this word somewhere". The
# file is generated one JSON token per line, which is what makes a member of the record union
# findable as the object a brace opens at the union's own indentation.
schema_record_carries() {
	awk -v kind="$1" -v field="${2:-}" '
		/^    \{$/ { block = "" }
		{ block = block $0 "\n" }
		/^    \},?$/ {
			if (block ~ ("\"const\": \"" kind "\"") &&
				(field == "" || block ~ ("\"" field "\": [{]"))) {
				found = 1
			}
			block = ""
		}
		END { exit found ? 0 : 1 }
	' "$RECORD_CONTRACT"
}

# --- reading a projection export ------------------------------------------------------------------
#
# The export is one row per line, `<table> <hex key> <json>`, with `lsn` and `crc` as its first two
# lines. Everything below reads it by the table name in column one, never by line number, so a
# header line added or removed does not silently change what is being read.

# Each table in an export with the number of rows it holds: the shape the control arm
# compares. Positive statements over named things — a field added to a row cannot make this red for
# a reason that is not a defect, and a table it fails to mention is visibly unasserted.
export_shape() { awk '$1 != "lsn" && $1 != "crc" { print $1 }' "$1" | sort | uniq -c; }

# The `status` of every `goals` row, in key order. One line per goal, so two runs with a different
# number of goals differ here as well as in `export_shape`.
goal_status() { grep '^goals ' "$1" | sed -n 's/.*"status":"\([a-z_]*\)".*/\1/p'; }

# The `last_result` object the daemon attributed to the goal: `{pass, provenance, lsn}`, or empty
# where no predicate has run. This is the daemon's own statement of what closed the goal.
goal_last_result() { grep '^goals ' "$1" | grep -o '"last_result":{[^}]*}' | head -1; }

# The `reissued` flag of every `turns` row, in key order, as one line.
turn_reissued_flags() { grep '^turns ' "$1" | grep -o '"reissued":[a-z]*' | tr '\n' ' '; }

# The turns a dump shows as issued and never spawned — the state a crash between the durable write
# and the launch leaves, and the state replay has to resolve. One id per line.
unspawned_turns() {
	issued=$(records_of "$1" turn_issued | grep -o '"turn":"[^"]*"' | sort -u)
	spawned=$(records_of "$1" turn_spawned | grep -o '"turn":"[^"]*"' | sort -u)
	printf '%s\n' "$issued" | while read -r turn; do
		[ -n "$turn" ] || continue
		printf '%s\n' "$spawned" | grep -qxF "$turn" || printf '%s\n' "$turn"
	done
}


# --- the assertions every arm shares ------------------------------------------------------------

# The goal reached Done, and the predicate result that got it there was one the daemon observed.
#
# Two propositions, and the second is not implied by the first: a run that reached `Done` on an
# agent's self-report is a failing gate that looks like a passing one.
#
# # Which result caused it is read off the daemon, never re-derived here
#
# The `goals` row publishes `last_result` — `{pass, provenance, lsn}`, the predicate result the fold
# itself attributed to this goal — so the harness asks the projection which record it was. It does
# **not** take "the highest-lsn `predicate_result` below the `Done` decision". That inference is
# exact while one goal is in flight and selects the wrong record the moment two are, which is the
# shape two agents invite; and a harness carrying its own copy of the daemon's causation rule goes
# on passing after that rule changes, which is the anti-pattern the README describes in miniature.
#
# The named record is then found in the **journal** and checked there too, so the claim rests on two
# independent renderings of the same event rather than on the projection agreeing with itself.
assert_done_is_observed() {
	dump=$1
	export_doc=$2
	header "the goal reached Done"
	done_lsn=$(records_of "$dump" loop_decided | grep -F '"kind":"done"' | awk '{ print $1 }' | tail -1)
	if [ -z "$done_lsn" ]; then
		fail "no loop decision was Done"
		note "decisions seen: $(records_of "$dump" loop_decided | grep -o '"kind":"[a-z_]*"' | sort -u | tr '\n' ' ')"
		return
	fi
	pass "a loop decision was Done, at lsn $done_lsn"

	header "the predicate result the daemon names as the goal's carries Provenance::Observed (invariant 3)"
	last_result=$(goal_last_result "$export_doc")
	if [ -z "$last_result" ]; then
		fail "the goals row names no last_result, so nothing says what closed the goal"
		return
	fi
	causing_lsn=$(printf '%s' "$last_result" | sed -n 's/.*"lsn":\([0-9][0-9]*\).*/\1/p')
	if printf '%s' "$last_result" | grep -qF '"provenance":"observed"' &&
		printf '%s' "$last_result" | grep -qF '"pass":true'; then
		pass "the goals row names lsn $causing_lsn, an observed pass"
	else
		fail "the result the goals row names is not an observed pass"
		note "$last_result"
	fi

	header "and the journal says the same about that record, read independently of the projection"
	causing=$(awk -v at="$causing_lsn" '$1 == at && $2 == "predicate_result"' "$dump")
	if [ -z "$causing" ]; then
		fail "no predicate_result at lsn ${causing_lsn:-unknown}, which the goals row names"
	elif [ "$causing_lsn" -ge "$done_lsn" ]; then
		fail "the named result at lsn $causing_lsn is not below the Done decision at $done_lsn"
	elif printf '%s' "$causing" | grep -qF '"provenance":"observed"' &&
		printf '%s' "$causing" | grep -qF '"pass":true'; then
		pass "the record at lsn $causing_lsn is an observed pass, below the Done decision"
	else
		fail "the record the goals row names is not an observed pass in the journal"
		note "$causing"
	fi

	header "no predicate result was Inferred"
	inferred=$(records_of "$dump" predicate_result | grep -cF '"provenance":"inferred"')
	if [ "$inferred" -eq 0 ]; then
		pass "every predicate result the daemon journaled, it ran"
	else
		fail "$inferred predicate result(s) claim a provenance the daemon cannot have"
	fi
}

# The control on the assertion above: the predicate really can come out false, so `Done` is not
# structural. Without it, "the deciding command came out true" is compatible with a daemon that
# never runs one.
#
# **Its own function because it is a statement about the *run*, not about the daemon**: it needs a
# run in which a turn ends without meeting the goal, which is two turn-ends. Every arm that has two
# calls it. An arm whose run has one turn-end by construction — the post-`Spawn` arm, whose two
# children belong to a single turn — cannot satisfy it and says so where it would have called it,
# rather than passing a weaker version of it.
assert_a_predicate_came_out_false() {
	header "the control: the predicate really can come out false, so Done is not structural"
	earlier=$(records_of "$1" predicate_result | grep -cF '"pass":false')
	if [ "$earlier" -ge 1 ]; then
		pass "$earlier earlier predicate result(s) were observed and false"
	else
		fail "no predicate result was ever false — this arm cannot tell Done from a daemon that never checks"
	fi
}

# Every issued turn was spawned exactly once — not zero, not twice.
assert_one_spawn_per_issued_turn() {
	dump=$1
	header "each issued turn was spawned exactly once"
	issued=$(records_of "$dump" turn_issued | grep -o '"turn":"[^"]*"' | sort)
	spawned=$(records_of "$dump" turn_spawned | grep -o '"turn":"[^"]*"' | sort)
	if [ -z "$issued" ]; then
		fail "no turn was issued at all"
		return
	fi
	bad=0
	for turn in $issued; do
		count=$(printf '%s\n' "$spawned" | grep -cxF "$turn")
		[ "$count" -eq 1 ] || {
			bad=$((bad + 1))
			note "$turn was spawned $count time(s)"
		}
	done
	if [ "$bad" -eq 0 ]; then
		pass "$(printf '%s\n' "$issued" | wc -l | tr -d ' ') issued turn(s), one spawn each"
	else
		fail "$bad turn(s) were not spawned exactly once"
	fi
}

# The repeating-failure loop, which no other clause of arm 2 can see: a goal that keeps
# re-issuing until the hop cap runs out ends `Exhausted{Hops}` while every other assertion here
# still holds.
assert_not_exhausted_by_hops() {
	dump=$1
	header "the goal did not end Exhausted{Hops}"
	hops=$(records_of "$dump" loop_decided | grep -cF '"reason":"hops"')
	if [ "$hops" -eq 0 ]; then
		pass "no decision was Exhausted{Hops}"
	else
		fail "$hops decision(s) were Exhausted{Hops}: the loop repeated instead of closing"
	fi
}

# --- driving the loop ----------------------------------------------------------------------------

# Hire the two agents and set the one goal, then leave the loop to it.
hire_two_and_set_one_goal() {
	hire_two
	set_one_goal
}

# Hire the two agents, and name the one a goal will go to as `GOAL_AGENT`.
#
# The second agent is not decoration: it never
# gets a goal, so it never takes a turn, and it is what makes "exactly one child" a statement about
# the goal rather than about how many agents exist.
hire_two() {
	header "two agents are hired, each with a role, and neither starts a turn"
	before=$(live_children) || fatal "could not collect children before hiring"
	builder=$(hire "kim" "builder" "infra") || fatal "the hire was refused"
	REVIEWER_AGENT=$(hire "leo" "reviewer" "infra") || fatal "the second hire was refused"
	after=$(live_children) || fatal "could not collect children after hiring"
	if [ "$before" -eq 0 ] && [ "$after" -eq 0 ]; then
		pass "hired $builder (builder) and $REVIEWER_AGENT (reviewer); no child before a goal exists"
	else
		fail "a hire with no goal launched something ($before before, $after after)"
	fi
	GOAL_AGENT=$builder
}

# The one goal, on `GOAL_AGENT`, carrying the predicate every arm is judged by.
#
# `test -f <marker>` — a program on the daemon's PATH, resolved and canonicalized by the daemon,
# given as a typed argv after `--`. There is no shell anywhere on this path.
issue_the_goal() {
	cli goal --agent "$GOAL_AGENT" --text "create the marker the goal is judged by" \
		--usd 1 --hop-cap 4 --timeout-s 30 \
		-- test -f "$WORK_DIR/done"
}

set_one_goal() {
	header "one goal, carrying a command predicate the daemon will run"
	issue_the_goal >/dev/null || fatal "the goal was refused"
	pass "goal set on $GOAL_AGENT, predicate: test -f $WORK_DIR/done"
}

# The negative two-control gives the otherwise idle reviewer one goal only after run 2 is held.
# It makes the same exact-one assertion see two direct children; it does not change the production
# spawn path or try to manufacture a second replay.
set_reviewer_goal_for_child_count_control() {
	header "the zero/two control deliberately starts the reviewer's child"
	GOAL_AGENT=$REVIEWER_AGENT
	issue_the_goal >/dev/null || fatal "the reviewer's control goal was refused"
	pass "control goal set on $GOAL_AGENT"
}

# The same goal, from a call that will never be answered.
#
# **Only for an arm that stalls the daemon on the launch its own `goal.set` causes.** The decide
# loop is what answers an operator call and what runs the `Spawn` effect that call produces, in
# that order and on one thread, so a fault armed on the *first* launch parks the loop with the
# reply unsent. That is not a defect and this is not a workaround for one: it is what stopping the
# loop means. The call is therefore detached and the arm synchronises on the marker and on the
# journal instead — and, because `TurnIssued` was fsynced before the effect ran, the goal is
# durable whether or not anything ever hears back. The arm asserts that rather than assuming it.
set_one_goal_detached() {
	header "one goal, set from a call that cannot be answered"
	issue_the_goal >"$SANDBOX/goal.out" 2>&1 &
	GOAL_CLI_PID=$!
	note "goal.set is in flight as pid $GOAL_CLI_PID, and the loop that would answer it is about"
	note "to stop inside the launch it causes"
}

# Wait for that detached call to end, once the daemon it was waiting on is gone.
#
# Its exit status says nothing worth asserting — a client whose daemon was `SIGKILL`ed mid-call
# fails, and that is the arm working. What this is for is leaving no client holding a socket in a
# sandbox the script is about to remove.
await_detached_goal_ends() {
	[ -n "${GOAL_CLI_PID:-}" ] || return 0
	waited=0
	while kill -0 "$GOAL_CLI_PID" 2>/dev/null; do
		waited=$((waited + 1))
		if [ "$waited" -gt 300 ]; then
			kill -9 "$GOAL_CLI_PID" 2>/dev/null
			break
		fi
		sleep 0.1
	done
	wait "$GOAL_CLI_PID" 2>/dev/null
	GOAL_CLI_PID=""
}

# hire <name> <role> <department> — prints the new agent id.
# hire <name> <role> <department> — prints the new agent id, or fails.
#
# The pipeline's status is the `sed`'s, which is 0 whether or not the CLI answered, so the caller's
# `|| fatal` could never fire on a refused hire. The emptiness check is what makes the status mean
# something: no id printed, no agent hired.
hire() {
	id=$(cli --json hire --name "$1" --role "$2" --department "$3" \
		--provider claude --cwd "$WORK_DIR" --usd-per-day 5 |
		sed -n 's/.*"agent":"\([^"]*\)".*/\1/p')
	[ -n "$id" ] || return 1
	printf '%s\n' "$id"
}

# Wait for the stand-in's run $1 to be holding, and say what was counted while it was.
#
# The hold is what makes the count exact: the child is alive and blocked, so the number cannot be
# read a moment before the process exists or a moment after it exits.
await_standin() {
	await "the stand-in's run $1 to start" 60 file_exists "$WORK_DIR/started.$1"
}

# --- the control arm's own job: comparing the two runs ---------------------------------------------

# The control, run from `crash.sh --no-crash` with the crashed run's kept artefacts beside it.
#
# What the control is for: "exactly one child after replay" cannot, on its own, tell replay
# re-issuing one turn from a harness that only ever produces one child. Only a comparison with a
# run in which the kill did not happen can.
compare_with_the_crashed_run() {
	mine=$1
	theirs="$GATE_OUT/crash.journal"
	at_kill="$GATE_OUT/crash.at-kill.journal"

	header "the crashed run being compared against is this attempt's, not one left lying around"
	# `GATE_OUT` is a fixed directory and nothing clears it, so a crashed run from last week reads
	# exactly like one from a minute ago and every comparison below passes against it — including
	# "the crashed run had one turn written and unlaunched", which is then a statement about a file
	# rather than about anything this attempt did. Two independent guards, because either alone has
	# a hole: a **freshness** bound catches artefacts from an earlier session, and **single use**
	# catches this control being run twice against one crashed run.
	attempt="$GATE_OUT/crash.attempt"
	consumed="$GATE_OUT/no-crash.consumed"
	if [ ! -r "$theirs" ] || [ ! -r "$at_kill" ] || [ ! -r "$attempt" ]; then
		fail "no crashed run to compare with — run ./crash.sh before ./crash.sh --no-crash"
		return
	fi
	finished=$(awk '{ print $1 }' "$attempt")
	attempt_id=$(awk '{ print $2 }' "$attempt")
	age=$(($(date +%s) - finished))
	if [ "$age" -lt 0 ] || [ "$age" -gt "$MAX_ATTEMPT_AGE" ]; then
		fail "the crashed run finished ${age}s ago, past the ${MAX_ATTEMPT_AGE}s bound"
		note "re-run ./examples/two-roles/crash.sh, then this control"
		return
	fi
	if [ -r "$consumed" ] && grep -qxF "$attempt_id" "$consumed"; then
		fail "crashed run $attempt_id has already been compared against once"
		note "each control run needs its own crashed run: re-run ./examples/two-roles/crash.sh"
		return
	fi
	printf '%s\n' "$attempt_id" >>"$consumed"
	pass "crashed run $attempt_id, finished ${age}s ago"

	header "the two runs produce different journals, and the difference is the one that matters"
	# The difference is named rather than left to a file comparison: the crashed run
	# carries a durable TurnIssued with no Spawn before its restart, and this one never does.
	# That is the check. A bare `diff` of the two dumps would report "different" for two runs that
	# were identical in every way that matters, because every id in them is freshly minted.
	theirs_unspawned=$(unspawned_turns "$at_kill" | grep -c . || true)
	mine_unspawned=$(unspawned_turns "$mine" | grep -c . || true)
	if [ "$theirs_unspawned" -eq 1 ] && [ "$mine_unspawned" -eq 0 ]; then
		pass "the crashed run had one turn written and unlaunched; this run never had any"
	else
		fail "unspawned turns: crashed $theirs_unspawned (want 1), no-crash $mine_unspawned (want 0)"
	fi

	header "and the crashed run says so in the record replay wrote"
	theirs_reissued=$(records_of "$theirs" turn_spawned | grep -cF '"reissued":true')
	mine_reissued=$(records_of "$mine" turn_spawned | grep -cF '"reissued":true')
	if [ "$theirs_reissued" -eq 1 ] && [ "$mine_reissued" -eq 0 ]; then
		pass "one replayed spawn in the crashed run, none in this one"
	else
		fail "replayed spawns: crashed $theirs_reissued (want 1), no-crash $mine_reissued (want 0)"
	fi

	header "the control on the comparator: it reads the journals rather than answering a constant"
	# The same function over two dumps of the **same** crashed run — before its restart and after —
	# must give different answers: one turn unlaunched at the kill, none once replay has spawned
	# it. Without this, an `unspawned_turns` that always answered 1 (or always 0) would satisfy
	# every assertion above.
	theirs_after=$(unspawned_turns "$theirs" | grep -c . || true)
	if [ "$theirs_unspawned" -eq 1 ] && [ "$theirs_after" -eq 0 ]; then
		pass "the crashed run: 1 unlaunched turn at the kill, 0 after replay"
	else
		fail "the crashed run reads $theirs_unspawned at the kill and $theirs_after after replay"
	fi

	header "the same one child: each run spawned exactly one process per turn it issued"
	theirs_issued=$(records_of "$theirs" turn_issued | grep -c .)
	theirs_spawned=$(records_of "$theirs" turn_spawned | grep -c .)
	mine_issued=$(records_of "$mine" turn_issued | grep -c .)
	mine_spawned=$(records_of "$mine" turn_spawned | grep -c .)
	if [ "$theirs_issued" -eq "$theirs_spawned" ] && [ "$mine_issued" -eq "$mine_spawned" ] &&
		[ "$theirs_issued" -eq "$mine_issued" ]; then
		pass "$theirs_issued issued and $theirs_spawned spawned in both runs"
	else
		fail "crashed $theirs_issued issued / $theirs_spawned spawned; no-crash $mine_issued / $mine_spawned"
	fi

	compare_the_two_projections
}

# What the two runs' **projections** must agree on, and what they must not.
#
# The two exports are not compared for equality, and that is not a concession: a crash is meant to
# stay visible in the projection afterwards, so the crashed run's turn row reads `reissued: true`
# where the control's reads `false`. A gate demanding those match would be demanding the loss of the
# one thing arm 2 exists to observe. Every identifier is minted per run besides.
compare_the_two_projections() {
	theirs="$GATE_OUT/crash.export.store"
	mine="$GATE_OUT/no-crash.export.store"

	if [ ! -r "$theirs" ] || [ ! -r "$mine" ]; then
		header "the two runs' projection exports"
		fail "one of the two exports is missing: ${theirs} / ${mine}"
		return
	fi

	header "the two runs' exports carry the same tables"
	theirs_tables=$(export_shape "$theirs" | awk '{ print $2 }')
	mine_tables=$(export_shape "$mine" | awk '{ print $2 }')
	if [ "$theirs_tables" = "$mine_tables" ]; then
		pass "$(printf '%s' "$mine_tables" | tr '\n' ' ')"
	else
		fail "the two exports differ in their table inventory"
	fi

	header "ordinary tables carry the same row counts"
	theirs_shape=$(export_shape "$theirs" | grep -Ev ' (session_totals|instructions)$')
	mine_shape=$(export_shape "$mine" | grep -Ev ' (session_totals|instructions)$')
	if [ "$theirs_shape" = "$mine_shape" ]; then
		pass "$(printf '%s' "$mine_shape" | tr -s ' \n' ' ')"
	else
		fail "the two exports differ in their tables or in how many rows each holds"
		note "crashed: $(printf '%s' "$theirs_shape" | tr -s ' \n' ' ')"
		note "control: $(printf '%s' "$mine_shape" | tr -s ' \n' ' ')"
	fi

	header "instruction cells match their exact journal boundaries, including the uncertain crash interval"
	if (cd "$REPO_ROOT" && cargo run --quiet --release -p xtask -- gate-instructions \
		--crash-journal "$GATE_OUT/crash.journal" --crash-export "$theirs" \
		--control-journal "$GATE_OUT/no-crash.journal" --control-export "$mine"); then
		pass "the crash retains its earlier Unknown interval and the same immutable turn's completed retry"
	else
		fail "instruction cells or their crash/retry boundaries disagree"
	fi

	header "the crashed run's session_totals holds one row more: the session its re-issue ran on"
	reissued_on=$(records_of "$GATE_OUT/crash.journal" turn_spawned | grep -F '"reissued":true' |
		sed -n 's/.*"session":"\([^"]*\)".*/\1/p')
	reissued_key=$(printf '%s' "$reissued_on" | od -An -tx1 | tr -d ' \n')
	theirs_sessions=$(grep -c '^session_totals ' "$theirs")
	theirs_others=$(awk -v key="$reissued_key" '$1 == "session_totals" && $2 != key { n++ }
		END { print n + 0 }' "$theirs")
	mine_sessions=$(grep -c '^session_totals ' "$mine")
	if [ -n "$reissued_on" ] && [ "$theirs_sessions" -eq $((mine_sessions + 1)) ] &&
		[ "$theirs_others" -eq "$mine_sessions" ]; then
		pass "crashed $theirs_sessions, control $mine_sessions; the extra row is $reissued_on"
	else
		fail "session_totals rows: crashed $theirs_sessions, control $mine_sessions"
		note "the re-issue ran on ${reissued_on:-no session}; $theirs_others crashed rows are other sessions"
	fi

	header "and both runs' goals stand at the same status"
	theirs_status=$(goal_status "$theirs")
	mine_status=$(goal_status "$mine")
	if [ -n "$mine_status" ] && [ "$theirs_status" = "$mine_status" ]; then
		pass "both runs' goals are $(printf '%s' "$mine_status" | tr '\n' ' ')"
	else
		fail "goal status differs: crashed [${theirs_status:-none}], control [${mine_status:-none}]"
	fi

	header "both runs end in the same terminal state"
	theirs_done=$(records_of "$GATE_OUT/crash.journal" loop_decided | grep -cF '"kind":"done"')
	mine_done=$(records_of "$GATE_OUT/no-crash.journal" loop_decided | grep -cF '"kind":"done"')
	if [ "$theirs_done" -eq 1 ] && [ "$mine_done" -eq 1 ]; then
		pass "each run reached Done exactly once"
	else
		fail "Done decisions: crashed $theirs_done, no-crash $mine_done; each should be 1"
	fi

	# The measurement behind the paragraph above, printed so a reader can check the reasoning rather
	# than take it. The interpretation is **computed from the two columns**, not asserted beside
	# them, so the `reissued` conclusion cannot be printed in a run where both columns read `false`.
	header "measurement: what the two exports share, and what they do not"
	sort "$theirs" >"$GATE_OUT/.a"
	sort "$mine" >"$GATE_OUT/.b"
	shared=$(comm -12 "$GATE_OUT/.a" "$GATE_OUT/.b" | grep -c . || true)
	rm -f "$GATE_OUT/.a" "$GATE_OUT/.b"
	theirs_flags=$(turn_reissued_flags "$theirs")
	mine_flags=$(turn_reissued_flags "$mine")
	note "$shared of $(wc -l <"$mine" | tr -d ' ') lines are shared by the two exports"
	note "crashed run's turn rows: ${theirs_flags:-none}"
	note "control run's turn rows: ${mine_flags:-none}"
	if [ "$theirs_flags" != "$mine_flags" ]; then
		note "the two differ on re-issue, which is why equality is the wrong comparison: a"
		note "projection where these matched would have lost that a turn was taken up by recovery."
	else
		note "the two agree on re-issue in this pair of runs, so the difference between the"
		note "exports here is per-run identity alone."
	fi
}
