# Zeron Personal Windows Builds

This public fork of [zeronsh/zeron](https://github.com/zeronsh/zeron) keeps the
app named Zeron. The `patched-windows` branch starts from upstream `v0.2.101`
(`b42fc2b8fbf247dd92796c2917f277535cea91ac`) and incorporates upstream `main`
through `d5c1cdc1` (version `v0.2.104`). It is a personal build, not an official
Zeron release.

## Changes

- Open the new-chat canvas by default after the chat list loads. Turn off
  **Start with a new chat** in **Settings > General** to open the most recently
  active conversation instead on the next startup. Sidebar selections,
  keyboard navigation, and conversation deep links still work normally.
- Disable automatic desktop update downloads and installation on quit so an
  official binary cannot silently replace these patches. Update notifications
  are unchanged; install future patched builds through this workflow.
- Choose, change, or remove a profile image through the account menu. The
  native picker selects a file on this computer; Zeron saves a small square
  copy in its local data directory and remembers it separately for each
  account. Images stay on this device and are not uploaded or synced.
- Remove trailing blank lines when sending or saving queued messages, but only
  when the Markdown parser produces identical content. Preserve indentation,
  spaces on nonblank lines, internal paragraph breaks, and meaningful code or
  raw-HTML whitespace. Failed sends still restore the original draft.
- Discard the interrupted Claude turn's held completion once a steer is
  confirmed, so quiet tools in the new turn cannot trigger a stale end or
  prematurely drain queued messages. Preserve the fallback for steers the
  CLI never confirms and let active tools finish normally. Remote chats need
  this fix on the engine running Claude; a Windows rebuild alone does not
  update the Ubuntu engine.
- Show structured asynchronous Codex questions in a separate panel with choices
  and custom answers, without replacing the normal chat draft. The panel uses
  the composer's theme-aware frosted-glass material. Questions stay answerable
  after the turn finishes; delivery errors preserve answers for retry. Remote
  chats require the patched engine on both devices. Installing this Windows
  build alone does not update the Ubuntu host.
- Show a persistent turn-duration footer beneath completed assistant replies
  in normal mode, using the full stored turn duration, including tool work and
  waiting for input. Include the local completion time, for example
  `Baked for 19s · done 9:07 PM`. Vary the prefix between **Baked**, **Cooked**,
  **Worked**, **Brewed**, **Crafted**, and **Simmered**, with a stable choice per
  turn that stays the same when reopening the chat. Compact mode uses the same
  format and prefix on its existing duration label. Omit the completion time
  when it cannot be derived from stored timing; older turns without a stored
  duration remain unchanged.
- Order the composer footer as TPS, account usage, then context usage. Keep
  the original separate account/context progress rings and percentages,
  including their warning colors and individual popovers. The TPS speedometer
  chip opens a themed token-usage popover with recorded thread totals for
  input, output, total, cached input, cache writes, reasoning, and
  provider-reported USD cost where available. The cache-hit rate uses complete
  reported input/cache counts; Average TPS stays specific to the latest turn.
  Reopening a thread restores its recorded totals without counting repeated
  snapshots or joined continuations twice.
  Cache scalar footer statistics between relevant transcript updates; text-only
  streaming and unrelated renders do not rescan history. Average TPS is output
  tokens divided by the entire turn's wall-clock duration, including startup, tools,
  and waiting; it is not raw model-generation speed. Keep the previous turn's
  rate until the new turn reports output instead of showing **Measuring**.
  Stream reported counts into the footer during the turn. Update the live
  average with each new count/duration snapshot and hold that reading between
  reports, including during tool and input waits, without additional provider
  requests. The final average uses the full turn duration. The popover
  distinguishes live, previous, and completed-turn rates. Tool/thinking text
  never substitutes for token telemetry. Show **Not reported** for unavailable
  fields, and retain earlier reported counts when a later turn
  omits them. Cache is included in
  input and reasoning in output, never added again to the totals.
  Persist Claude, Codex, and OpenCode reports with each assistant turn so
  reopening and device sync retain the selected thread's own statistics.
  Codex computes differences between session totals across requests, rather
  than mistaking the latest model
  request for the whole turn. In-place Codex steers without a separate provider
  usage boundary leave TPS unreported rather than guessing an allocation.
  OpenCode sums completed main-session requests across each turn on both
  server protocols, including cache and reasoning in the normalized counts.
  Claude tracks streaming usage by API message ID across repeated content
  blocks; its final result replaces provisional totals, never adds to them.
  OpenCode exposes completed-request usage during a still-running turn.
  Repeated request snapshots and late reports from retired turns do not
  inflate usage. An unfinished request leaves full-turn TPS unreported.
  Bound Claude's live request tracking to 4096 IDs per warm session. If that
  cap or the ID-size limit is exceeded, discard live tracking and use final
  provider reports until the session ends, rather than readmitting stale IDs.
  Older providers without trustworthy turn totals show no TPS. Existing
  history is untouched and not retroactively estimated.
  Remote chats require this patch on the engine running the agent; installing
  the Windows app alone does not update the Ubuntu host.
- Notify once when a new asynchronous question request arrives in the open
  conversation, even while Zeron is minimized or in the background. Use the
  existing **Input required** sound preference and desktop notification
  settings; replayed history, repeated updates, and answering a request do
  not alert again. Windows uses the question chime because upstream's Windows
  desktop-toast handler is not implemented. This is a UI-only change; the
  Ubuntu engine does not need an update.
- Prefer the latest request's measured generation TPS in the footer when
  codex-lb reports it. Match its dashboard formula: output minus reasoning,
  divided by elapsed time minus time to first token. Keep whole-turn average
  TPS in the token popover, and retain the existing, clearly labeled average
  for providers without request timing. Hold readings between reports.
  The host must explicitly opt in through
  `$CODEX_HOME/zeron-codex-lb-usage.toml` (`~/.codex` by default) with
  `enabled = true` and `origin = "http://your-codex-lb:2455"`.
  The selected `codex-lb` provider must use `CODEX_LB_API_KEY` and exactly that
  origin. Requires codex-lb's read-only
  `GET /v1/responses/{response_id}/metrics` integration endpoint. Lookup uses
  the same API key and exact response IDs from this thread's Codex rollout,
  never another conversation's latest request. Only bounded new rollout data
  is read; prompts and keys are never sent to the metrics endpoint or persisted
  in token statistics. Redirects are disabled, lookups have short timeouts,
  and telemetry failures do not fail the chat. Persist generation measurements
  with the turn for reconnects and device sync. Unsupported/older Codex
  rollouts keep the prior average display; existing history is not remeasured.
  Reported cost is unchanged and is not inferred from codex-lb pricing.
- Use the earlier whole-turn average TPS for Claude Code and OpenCode, not
  client-arrival generation estimates: buffered CLI output is not a reliable
  measure of model-generation time. Ignore previously saved client estimates
  in both live and completed-turn displays, without changing stored chats or
  token/cache/cost totals. Keep codex-lb's server-measured generation TPS,
  bounded Claude request tracking, and cached footer statistics. Remote use
  requires an updated Ubuntu engine as well as the Windows app.
- Retain upstream's `Shift+Backspace` fix: holding Shift while pressing
  Backspace still deletes backward or removes the selected text in the
  composer and search inputs.
- Open the selected session's existing delete confirmation with
  `Ctrl+Shift+Backspace` on Windows/Linux (`Cmd+Shift+Backspace` on macOS).
  Rebind **Delete session** in **Settings > Shortcuts**. The shortcut never
  deletes immediately; press Enter or choose Delete to confirm, or press
  Escape or choose Cancel to dismiss. Holding Enter does not send the draft
  underneath the dialog. The shortcut does nothing on the new-chat canvas,
  in Settings, or beneath an open overlay.
  Keep normal `Ctrl+Backspace` word deletion unchanged, and preserve existing
  custom shortcuts when upgrading.
- Refresh account usage every minute while the window is active and every
  five minutes in the background. Completed turns also trigger a refresh,
  deferred when necessary to honor the 30-second cooldown. Automatic refreshes
  probe only the current harness's active accounts; opening the account picker
  still refreshes all saved accounts. Remote scoped polling requires the
  patched Ubuntu engine; provider backoff and last-good usage caches remain.
- Keep the app name, version, installer identity, account handling, and
  conversation storage unchanged.

## Build and Download

1. Open [Patched Windows Build](https://github.com/trkshi/zeron/actions/workflows/patched-windows.yml).
2. Choose **Run workflow** on the `patched-windows` branch.
3. After it succeeds, download the `zeron-windows-x86_64` artifact. It contains
   the normal Windows setup executable, portable ZIP, standalone executable,
   SHA-256 checksums, and build provenance. Artifacts expire after seven days.

The workflow uses a standard `windows-2022` GitHub-hosted runner. It compiles
and packages the application with upstream's locked dependencies and packaging
script; it does not run the upstream test suites or deployment workflows.

## Install Without Changing Conversation Data

1. Close the existing Windows app and back up `%LOCALAPPDATA%\Zeron`. If you
   configured `ZERON_DATA_DIR`, back up that directory instead. Keep the backup
   private: it includes account credentials and conversation data.
2. Run `zeron-0.2.104-windows-x86_64-setup.exe` to replace the existing app in
   place. There is no need to uninstall it first. Alternatively, extract the
   portable ZIP and run its `zeron.exe`, with the old app closed.
3. Use the same Windows user, Zeron account, and data-directory configuration.
   The startup change only changes which screen opens; it does not create,
   delete, move, or migrate any conversations. The Ubuntu engine is unchanged.

Do not restore an official executable or use its updater when you want to keep
the patches. The packaged update feed points to this fork, not the upstream
release downloads. Updates are delivered through workflow artifacts for now.
An upstream installer can restore the official app when needed; match its
version to avoid downgrading stored data.

## Bring In Upstream Updates

Keep `main` as the unmodified upstream branch and personal changes on
`patched-windows`. Configure remotes as `origin` = `trkshi/zeron` and
`upstream` = `zeronsh/zeron`.

Fetch upstream `main` and merge it into the patched branch. Merging preserves
patch commits without rewriting history or requiring a force push:

```bash
git fetch upstream main
git switch patched-windows
git merge upstream/main
# Resolve any conflicts while retaining the personal changes.
# Commit the resolved merge before pushing.
git push origin patched-windows
gh workflow run patched-windows.yml --repo trkshi/zeron --ref patched-windows
```

These commands update only this fork; they do not open an upstream pull request.
To follow stable releases instead, fetch and merge the chosen upstream tag
rather than `upstream/main`.

Do not use GitHub's **Sync fork** action to overwrite the patched branch.
Review upstream changes and any data migrations before installing a newer
build. Backups still matter when updating the upstream application itself.

Source changes and workflow logs are public. Never commit Zeron data,
credentials, personal avatars, or backups to this repository.
