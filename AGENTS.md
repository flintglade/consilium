# Operating Philosophy

## Prove it before calling it done
No task is finished until something verifies it — `cargo check`, `cargo test`, `cargo clippy`, or a manual run. I iterate until the check passes, then re-read my diff in a fresh pass before signing off.

## Simple code that fully works
Ship complete, polished work with the simplest code that satisfies the real requirement. Keep implementations readable, direct, and easy to debug or extend later. Simplicity is not permission to underbuild: keep the required edge cases, error paths, verification, and user experience. Avoid speculative abstractions, but add a small abstraction when it clearly reduces real complexity or matches the existing structure.

## Stay in scope
Change only what the task needs. One source of truth for each piece of state and logic.

## Clarify before building
For non-trivial features, resolve design questions one at a time before writing code. If the answer is in the repo, read it — don't ask.

## Write down corrections
When I repeat a mistake, add the lesson here so the next session doesn't relearn it.
