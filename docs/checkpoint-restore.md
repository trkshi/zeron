# Checkpoint Restore

Hover a user message and choose **Restore before this message**. The dialog offers:

- **Conversation:** create a new top-level thread containing the preceding transcript.
- **Files:** restore the project's saved working files without changing either transcript.
- **Both:** restore files, then create the new thread.

The original conversation stays intact. The restored thread starts a separate provider
session with inherited transcript context, not the original provider session's hidden state.
Both the desktop engine and the source device must advertise `checkpoints-v1`.

## Files and Safety

The host snapshots the Git checkout before admitting a new prompt, including idle
runtime sends and steering. A prompt delivered while an agent is working records an
unavailable checkpoint rather than presenting moving files as a stable snapshot.
Crash recovery and startup retries preserve the original snapshot, when one exists.
Older messages and expired snapshots support conversation restore only.

Snapshots include tracked files and non-ignored untracked files, not just agent edits.
They exclude ignored files, Git history/index changes, databases outside the checkout,
running processes, and other external effects. Symlinks, Windows reparse points,
submodules, unsafe paths, and oversized projects make file restore unavailable.
Changing Git HEAD or the branch also prevents file restore.

The dialog previews every file to create, remove, or restore. Confirmation rechecks
the preview and saves a recovery backup before modifying anything. Recovery backups
are selectable from the original thread's restore dialog.

File restore refuses active engine turns/background agents and unsaved UI editor
buffers. It retires idle provider runtimes, blocks new prompt admission and workspace
file writes during the operation, and checks files again before replacing them.
External editors, terminals, and other engines are not locked: stop those writers
before restoring. Multi-file replacement is not a filesystem transaction; failed
restores attempt rollback and retain a backup for recovery.

An operation UUID makes retries after a lost RPC reply idempotent. A worker retains its
locks even if the caller disconnects. Interrupted operations surface an error instead
of automatically replaying file mutations.

## Storage

Snapshots are host-local in `<profile store>/checkpoints/checkpoints.sqlite`; they are
not uploaded with conversation sync. Content is deduplicated by SHA-256.

- 20,000 files, 8 MiB per file, and 128 MiB per snapshot.
- 50 message snapshots and 10 recovery backups per conversation; 1,000 globally.
- 512 MiB content limit and 768 MiB SQLite page limit.
- The selected snapshot and unfinished restore backups are protected during pruning.

These are bounded recovery points, not a substitute for permanent project backups.

## Coverage

Focused tests live in `crates/engine/tests/checkpoints.rs`, the engine checkpoint
module, and the document/protocol/UI state modules. They use temporary projects and
mock harnesses, not live providers. They cover cold/warm prompt capture, steering,
preserved history and provider independence, recovery, stale previews, active-turn
refusal, retention, Git-index preservation, and unsafe paths. They have not been run
as part of this implementation.
