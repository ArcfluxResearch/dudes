<!-- Title: type(scope): description — feat | fix | refactor | test | docs | build | ci | chore | perf | bump | revert | localize;
     scope = crate or area (types, proto, journal, policy, supervisor, providers, mailbox, rpc, pty, proxy, xtask, floor, ci, docs, deps).
     No ticket id or badge in the title. Labels carry size/risk: oversize, needs-split, ci-change. -->

## Summary

<!-- one paragraph: what changed and why; link the issue it addresses, if any -->

## Verification

<!-- the exact commands run and their result, e.g.
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings --locked && cargo nextest run --workspace && cargo deny check
-->

## Checklist

- [ ] The change does what the description says, with the evidence above
- [ ] Nothing changed that the description or linked issue did not call for (out-of-scope work is a separate issue)
- [ ] Tests added or updated name the invariant they protect
- [ ] ≤ 400 changed lines (otherwise split)
- [ ] `CHANGELOG.md` Unreleased entry, if user-visible
- [ ] No version change (the release pull request bumps it)
- [ ] Every document stating the old fact was updated
