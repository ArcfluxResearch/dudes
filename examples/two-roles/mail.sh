#!/bin/sh
set -u

. "$(dirname "$0")/lib.sh"

require_binaries
command -v python3 >/dev/null 2>&1 || fatal "the mail example requires Python 3"
XTASK=${DUDES_XTASK:-$TARGET_DIR/release/xtask}
[ -x "$XTASK" ] || fatal "no $XTASK; run cargo build --release --workspace"
open_sandbox
start_daemon mail DUDES_EXAMPLE_MODE=mail DUDES_EXAMPLE_CLI="$DUDES_CLI"

if python3 "$EXAMPLE_DIR/mail.py" run "$DUDES_CLI" "$RUNTIME_DIR" "$WORK_DIR" "$HOME_DIR" "$CONFIG_ROOT"; then
	pass "the two agents sent, read, replied to and acknowledged their mail"
else
	fail "the mail journey did not finish"
	stop_daemon
	finish "mail round trip"
fi
stop_daemon

if (cd "$REPO_ROOT" && "$XTASK" journal --state "$STATE_DIR") >"$SANDBOX/mail.journal" &&
	(cd "$REPO_ROOT" && "$XTASK" export --state "$STATE_DIR") >"$SANDBOX/mail.store" &&
	python3 "$EXAMPLE_DIR/mail.py" verify "$SANDBOX/mail.journal" "$SANDBOX/mail.store" "$WORK_DIR"; then
	pass "the stopped daemon's journal and saved view confirm the mail round trip"
else
	fail "the saved mail records did not match the journey"
fi
finish "mail round trip"
