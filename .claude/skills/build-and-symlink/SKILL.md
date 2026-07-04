---
name: build-and-symlink
description: Rebuilds the agtx release binary and refreshes the ~/.local/bin/agtx symlink so manually testing agtx picks up the latest code. Use whenever the user says "build and symlink", "rebuild and symlink", "commit, build and symlink", or wants to manually test an agtx change after switching, merging, or rebasing a branch.
---

# Build and symlink agtx for manual testing

1. Report what is about to be built, so the user knows what they are testing:
   ```bash
   git status --short --branch
   ```
2. Build the release binary:
   ```bash
   cargo build --release
   ```
3. Point the test symlink at this checkout's binary (idempotent - safe to re-run even if it already points here):
   ```bash
   ln -sf "$(pwd)/target/release/agtx" ~/.local/bin/agtx
   ```
4. Confirm the result back to the user: which branch/commit was built, and that the symlink now resolves to it:
   ```bash
   ls -la ~/.local/bin/agtx
   ```

Note: `~/.local/bin` must be on `$PATH` for a bare `agtx` invocation elsewhere to pick up the rebuilt binary.
If the user is testing from a different git worktree (e.g. `.agtx/worktrees/{slug}`), run all four steps from that worktree's directory, not the main checkout - the symlink must point at the worktree whose code is actually being tested.
