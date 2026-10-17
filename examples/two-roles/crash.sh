#!/bin/sh
# --no-crash runs the ordinary completion control without any recovery request.
set -u

. "$(dirname "$0")/lib.sh"

# --- which of the two runs this is ---------------------------------------------------------------

CRASH=yes
CHILD_COUNT_CONTROL=none
for argument in "$@"; do
	case $argument in
	--no-crash) CRASH=no ;;
	--child-count-control=zero) CHILD_COUNT_CONTROL=zero ;;
	--child-count-control=two) CHILD_COUNT_CONTROL=two ;;
	--child-count-control=read-failure) CHILD_COUNT_CONTROL=read-failure ;;
	*)
		echo "usage: $0 [--no-crash] [--child-count-control=zero|two|read-failure]" >&2
		exit 64
		;;
	esac
done
if [ "$CRASH" = no ] && [ "$CHILD_COUNT_CONTROL" != none ]; then
	echo "--child-count-control requires the crash arm" >&2
	exit 64
fi
if [ "$CRASH" = yes ]; then
	MODE=crash
	TITLE="arm 2 — persist before spawn survives a kill"
else
	MODE=no-crash
	TITLE="the control — the same run without the crash"
fi

# The read-failure control does not run the arm. What it exercises is the Windows child-count
# collector's own failure branches — a receipt naming another pid, and child records that cannot be
# read after the PowerShell step succeeded — beside the two successful collections that give those
# refusals their meaning. No POSIX run of this arm reaches any of them, and a native run reaches the
# refusals only when something has already gone wrong. It needs the sandbox and nothing else: no
# release binary, no daemon, no CIM query.
if [ "$CHILD_COUNT_CONTROL" = read-failure ]; then
	open_sandbox
	check_windows_collector_failure_branches
	finish "the read-failure control — the Windows collector's failure branches"
fi

require_binaries
require_recovery_tools
open_sandbox

# --- the daemon this arm drives -------------------------------------------------------------------
#
# The crash needs the loop to stop at one exact instant: after a turn is durably written and before
# its process exists. That window is microseconds wide in the daemon's own code, so an outside
# killer cannot land in it and a script that retried until it did would be flaky by construction.
#
# This script builds the daemon with `--features fault-injection` and uses the wrapper that parks
# the loop before a chosen launch and touches a file to say it has. **It kills nothing.** The
# SIGKILL below is a real external signal, sent by this script exactly as a person would send it,
# and the on-disk state it lands on is the window's: a turn written and fsynced, no process.
#
# The feature is off in every ordinary build, so the released daemon does not contain the code —
# that is the compiler's statement, not a promise. Both runs of this script use the same
# feature-built binary, so the only difference between them is whether the fault is armed.
#
# It builds into a target directory of its own, so `target/release/duded` — what arm 1 runs — is
# never replaced by a binary with a fault in it.
FAULT_TARGET="$TARGET_DIR/two-roles-evidence/fault"
DUDED="$FAULT_TARGET/release/duded"

printf '%s\n' "$TITLE"
printf '  cli       %s\n' "$DUDES_CLI"
printf '  sandbox   %s\n' "$SANDBOX"
printf '  building  duded --features fault-injection into %s\n' "$FAULT_TARGET"
(cd "$REPO_ROOT" && CARGO_TARGET_DIR="$FAULT_TARGET" cargo build --quiet --release -p duded \
	--features fault-injection) || fatal "could not build the fault-injection daemon"
printf '  daemon    %s\n' "$DUDED"
[ -x "$DUDED" ] || fatal "no $DUDED after the build"

MARK_ON_RUN=${MARK_ON_RUN:-2}
STALL_MARKER="$SANDBOX/stalled"
# Which launch the fault stops before. `2` is the arm, and the number is reachable from outside so
# that a reader can check the two assertions guarding the child count are load-bearing rather than
# decorative. Measured, so nobody has to guess:
#
#   STALL_NTH=3  a launch this run never reaches, so the daemon never enters the window and the arm
#                fails there — "the daemon never reached the window; nothing to kill".
#   STALL_NTH=1  **hangs, and does not go red.** The fault parks the decide loop, and the loop is
#                what answers `goal.set`, so the operator call that would cause the first launch
#                never returns and the script waits on a CLI that will not finish. Arming the fault
#                before the first launch is therefore not a usable variation of this arm; it is
#                recorded here rather than left for the next person to rediscover.
STALL_NTH=${STALL_NTH:-2}
# What identifies this attempt to the control arm, so it cannot compare against a crashed run from
# an earlier session. The sandbox name is minted by `mktemp -d`, so it is unique per invocation.
ATTEMPT_ID=$(basename "$SANDBOX")

if [ "$CRASH" = yes ]; then
	start_daemon "$MODE" \
		DUDES_FAULT_STALL_BEFORE_SPAWN="$STALL_NTH" \
		DUDES_FAULT_STALL_MARKER="$STALL_MARKER"
else
	start_daemon "$MODE"
fi

header "the daemon is up and answering the operator"
if dudes_ping; then
	pass "system.ping answered"
else
	fail "the daemon did not answer system.ping"
fi

hire_two_and_set_one_goal

# --- turn 1: a child exists, and this is asserted before anything is killed -----------------------

header "a child existed BEFORE the kill"
if await_standin 1; then
	before_kill=$(live_children) || fatal "could not collect children before the kill"
	if [ "$before_kill" -ge 1 ]; then
		pass "$before_kill live child(ren) of the daemon, pid $(standin_pid 1)"
	else
		fail "no live child before the kill: every count after this would be vacuous"
	fi
else
	fail "the goal never produced a turn"
	finish "$TITLE"
fi
release_standin 1

# --- the kill, or not -----------------------------------------------------------------------------

if [ "$CRASH" = yes ]; then
	header "the loop reaches the window between the turn's durable write and its launch"
	if await "the daemon to reach the pre-launch window" 60 file_exists "$STALL_MARKER"; then
		pass "the daemon is in the window (it announced it at $STALL_MARKER)"
	else
		fail "the daemon never reached the window; nothing to kill"
		finish "$TITLE"
	fi

	header "nothing has been launched for the turn that is about to be lost"
	at_window=$(live_children) || fatal "could not collect children in the pre-kill window"
	if [ "$at_window" -eq 0 ]; then
		pass "no live child: turn 1's is reaped and turn 2's does not exist"
	else
		fail "expected no child in the window, counted $at_window"
	fi
	if [ "$CHILD_COUNT_CONTROL" = zero ]; then
		header "zero-control: the exact-one assertion rejects the proved empty window"
		assert_one_child_is_the_agents 1
		finish "$TITLE"
	fi

	header "SIGKILL"
	kill_daemon_hard
	pass "the daemon is gone; it ran no exit path and reaped nothing"

	at_kill="$GATE_OUT/$MODE.at-kill.journal"
	dump_journal >"$at_kill" || fatal "could not read the journal the crash left behind"
	note "the journal as the crash left it: $at_kill"
	cp -R "$STATE_DIR" "$SANDBOX/at-kill-state" || fatal "could not copy the state the crash left"

	header "the crash left a durable TurnIssued with no Spawn (invariant 2)"
	unspawned=$(issues_without_spawn "$at_kill")
	count=$(printf '%s' "$unspawned" | grep -c . || true)
	if [ "$count" -eq 1 ]; then
		pass "exactly one issued turn has no spawn: $unspawned"
	else
		fail "expected exactly one unspawned turn, found $count"
		note "${unspawned:-none}"
	fi

	header "the daemon restarts on the state the crash left"
	# The fault is not armed on the restart: what is under test now is replay, and a second stall
	# would only stop the thing being measured.
	start_daemon "$MODE-restart"
	pass "restarted on $STATE_DIR"
	inspect_missing_spawn "$at_kill" 2
	submit_fresh_repeat
fi

# --- after replay (or, in the control, simply after the first predicate) ---------------------------

if [ "$CRASH" = yes ]; then
	header "the consented repeat has one held child"
else
	header "the loop took its second turn: one child, not zero and not two"
fi
if await_standin 2; then
	if [ "$CHILD_COUNT_CONTROL" = two ]; then
		set_reviewer_goal_for_child_count_control
		if await_standin 3; then
			header "two-control: the exact-one assertion rejects two direct children"
			assert_one_child_is_the_agents 2
		else
			fail "the reviewer's deliberate control child never started"
		fi
		finish "$TITLE"
	fi
	assert_one_child_is_the_agents 2
else
	fail "no turn ran after this point"
	finish "$TITLE"
fi
release_standin 2

header "the goal closes"
if await "the goal to reach Done" 60 journal_shows '"kind":"done"'; then
	pass "the goal reached Done"
else
	fail "the goal never reached Done"
fi

if [ "$CRASH" = yes ]; then
	save_final_recovery_receipt
fi
stop_daemon

# --- what the journal and the projections say, read offline ---------------------------------------

dump="$GATE_OUT/$MODE.journal"
store_export="$GATE_OUT/$MODE.export.store"
replay_export="$GATE_OUT/$MODE.export.replay"
dump_journal >"$dump" || fatal "could not read the journal back"
dump_export store >"$store_export" || fatal "could not export the projection store"
dump_export journal >"$replay_export" || fatal "could not replay the journal into a projection"
note "journal kept at $dump"
note "projection exports kept at $store_export and $replay_export"

assert_done_is_observed "$dump" "$store_export"
assert_a_predicate_came_out_false "$dump"
assert_one_spawn_per_issued_turn "$dump"
assert_not_exhausted_by_hops "$dump"
if [ "$CRASH" = yes ]; then
	assert_recovery_repeat "$at_kill" "$dump" "$store_export"
elif recovery_facts ordinary "$dump" "$store_export"; then
	pass "ordinary control has source witnesses and no recovery events or authority"
else
	fail "ordinary control created recovery authority"
fi

header "the re-issue is recorded as one, and only where a crash happened"
reissued=$(records_of "$dump" turn_spawned | grep -cF '"reissued":true')
if [ "$CRASH" = yes ]; then
	if [ "$reissued" -eq 1 ]; then
		pass "one durable Spawn is marked reissued after explicit consent"
	else
		fail "expected one consented reissued Spawn, found $reissued"
	fi
elif [ "$reissued" -eq 0 ]; then
	pass "no Spawn is marked reissued in the ordinary control"
else
	fail "$reissued Spawn(s) are marked reissued in an ordinary run"
fi

# --- the projection export after replay equals a from-scratch replay's -------------------

header "the projection is rebuildable from the journal alone (invariant 1)"
if diff -u "$replay_export" "$store_export" >"$SANDBOX/export.diff"; then
	pass "the export after replay equals the export of a from-scratch replay ($(wc -l <"$store_export" | tr -d ' ') lines)"
else
	fail "the two exports differ"
	sed 's/^/      | /' "$SANDBOX/export.diff"
fi

header "the control for that comparison: the export moves when the projection does"
if [ "$CRASH" = yes ]; then
	behind="$GATE_OUT/$MODE.export.at-kill"
	dump_export_of "$SANDBOX/at-kill-state" >"$behind" ||
		fatal "could not export the store the crash left"
	if ! diff -q "$behind" "$store_export" >/dev/null; then
		pass "the store the crash left exports differently from the one the run ended with"
		note "kept at $behind"
	else
		fail "the older crash projection exports the same document as the completed run"
	fi
elif [ -r "$GATE_OUT/crash.export.store" ]; then
	if ! diff -q "$GATE_OUT/crash.export.store" "$store_export" >/dev/null; then
		pass "this run's projection exports differently from the crashed run's"
	else
		fail "two independent runs exported an identical projection, which cannot be"
	fi
else
	fail "no crashed run to compare exports with — run ./crash.sh first"
fi

# --- the control arm's own job: comparing the two runs ---------------------------------------------

if [ "$CRASH" = yes ]; then
	# Stamp the artefacts with when this run finished and which invocation produced them, so the
	# control arm can refuse a crashed run left behind by an earlier session. Written last, so a
	# run that died half way through leaves no stamp for the control to trust.
	printf '%s %s\n' "$(date +%s)" "$ATTEMPT_ID" >"$GATE_OUT/crash.attempt"
	note "stamped this attempt as $ATTEMPT_ID"
else
	compare_with_the_crashed_run "$dump"
fi

finish "$TITLE"
