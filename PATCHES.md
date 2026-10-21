# Zeron Personal Windows Builds

This public fork of [zeronsh/zeron](https://github.com/zeronsh/zeron) keeps the
app named Zeron. The `patched-windows` branch starts from upstream `v0.2.101`
(`b42fc2b8fbf247dd92796c2917f277535cea91ac`) and incorporates upstream `main`
through `c526d6f7` (version `v0.2.114`). It is a personal build, not an official
Zeron release.

## Changes

- Integrate upstream's LaTeX math rendering, session card side-chat and
  subagent lanes, sidebar hover cards, draggable sidebar section order, and
  iPhone Duo layout. Keep the Usage display setting beside the new math rows,
  and give the lanes card the same model-aware brand marks as the sidebar.

- Integrate upstream's native Luau visuals, bulk deletion of archived sessions,
  the sidebar Archived toggle, OpenCode v2 question forms, Cmd+K Recent Files
  fix, and Cursor Plus model defaults. Keep the Usage dashboard module beside
  the new visuals module.

- Integrate upstream's PR side panes, multi-entry file selection, per-device
  worktree locations, persistent browser profiles, and idle harness tab icons.
  Preserve the Usage page's titlebar and profile isolation, checkpoint restores'
  autosave freeze, Home account switching, and sidebar navigation animation.

- Integrate upstream's tabbed workspace search, session-info panel, adaptive
  panel layout, projectless scratch folders, quieter Providers page, and MCP
  worker limits. Preserve the Usage dashboard command in the new palette,
  account auto-switch controls, and host-driven quota refreshes. Keep Home
  panels anchored to the composer and expanded agent-update notices above them.

- Share activity-driven account usage refreshes in the host engine across all
  running threads and clients, with a 30-second per-account minimum, coalesced
  pending work, provider backoff, and a five-minute quiet-turn fallback. Stream
  snapshots to the Home account pool, footer, and Usage Limits view. Automatic
  probes target active logins only; explicit refreshes and auto-switch checks
  retain access to saved accounts. Older engines keep the previous UI polling.

- Add a **Usage** dashboard from the sidebar footer, with **Cost**, **Tokens**,
  and **Limits** views, device selection, 24-hour/7-day/30-day/90-day periods,
  provider trends, billing-category bars, model details, and pooled account
  limits. Persist completed-turn scalars on the host without subscribing to
  transcripts or duplicating message text. Repeated snapshots and forked
  history do not count twice, and checkpoint restores do not erase consumption.
  Keep reported costs separate from standard API-value estimates; unknown
  models and incomplete billing reports stay unpriced. Subscription quota
  percentages are account averages, not combined token capacity. Both the UI
  and host engine need this update. See [usage history](docs/usage-history.md)
  for pricing, history coverage, and privacy limits.
- Show **Working now** beneath the new-chat composer: live top-level threads
  across projects and devices, with model, branch, and elapsed turn time.
  Keep the card compact and aligned with the input surface; project and branch
  sit below the title, while device details remain available on hover.
  Click a row to open its conversation; waiting questions show **Needs input**.
  Exclude archived threads, hidden workers, queued sends, and stale live activity.
  Confirm background sends from host command receipts so leaving a thread
  before its transcript acknowledges the message cannot strand it as working.
  Keep finished, unread threads visible as **Done** until opened on any device;
  show their count separately from running threads and stop their animations.
  Scroll longer lists and keep the panel clear of bottom chrome. This is a
  UI-only change; no Ubuntu engine update is required.
- Add a compact **Account pool** beside **Working now**, with expandable
  Codex/Claude groups, active-account names, session/weekly usage bars, and
  read-only auto-switch status for the composer's selected device. Reset
  times and probe errors stay on hover. Share the existing accounts cache;
  use the host's shared refresh stream, with five-minute UI polling for older
  hosts, a manual refresh button, and the engine's existing cooldown.
  Keep unknown, failed, expired, and offline readings out of available counts.
  Use `#141414` home panels in dark mode, retain light-theme surfaces, and
  stack them on narrow windows. Account switching stays in Providers; the
  panel itself needs no engine update.
- Add Home stats panels under **Working now** and the account pool:
  **Git activity** (your non-merge commits on local branches over 14 days,
  per-day bars, lines added/removed, project filter), **Builds** (latest
  GitHub Actions runs of each project's `origin`, via `gh`, cached 60 s),
  **Devices** (CPU, memory, and data-disk use per online device, engine
  version and uptime, red memory bar under Linux memory pressure), and
  **Spend** (today's and seven-day cost by provider, linking to Usage), and
  **Running services** (ports your processes listen on, labelled by project;
  Projects/All ports switch; local ports open in the browser; Linux and
  macOS hosts only). Panels after Working now fill the shorter of two
  columns. The summary pill counts commits; **Customize** and the new
  **Settings → Home** page hide any panel but Working now. New forwardable
  `GetDeviceHealth`, `GetGitActivity`, `ListCiRuns`, and
  `ListListeningServices` engine RPCs back them, so every device needs the
  engine update. Devices whose engine predates an RPC are left out of that
  panel instead of showing "unknown method", and re-checked every 10 minutes.
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
- Retain upstream's Claude/Codex lifecycle-based steering and stop handling.
  Reset measured message segments only at confirmed steering boundaries, not
  when a prompt is merely written to the provider. Quiet tools cannot release
  an old turn's completion. Remote chats need the updated engine too.
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
- Remove Zeron's codex-lb-specific metrics lookup and private pool-usage
  adapter. No rollout tailing, proxy-metrics polling, or proxy-specific account
  is needed for token statistics. Codex's configured provider and credentials
  are unchanged, as is the standalone proxy service. Historical recorded
  statistics remain readable; new Codex turns use the normal whole-turn average.
- Measure generation TPS per model request for Claude Code and Codex. Claude
  times each request from `message_start` to `message_stop`, so hidden
  thinking time pairs with the thinking tokens it reports. Codex times its
  visible output from the first agent-message delta to the request's usage
  report and leaves out hidden reasoning tokens. Waiting for the first token
  and tool runs fall outside every window. A request shorter than 300 ms or
  with fewer than 3 streamed chunks arrived as a burst and counts toward
  neither tokens nor time. OpenCode uses its server's own timestamps on both
  protocols: each request runs from its first text or reasoning start to its
  last text or reasoning end or tool call, so tool runs inside a request stay
  out. Without a streamed reasoning part, hidden reasoning tokens are left
  out. The footer shows generation TPS when a turn has a
  measured request and the whole-turn average otherwise; the popover keeps
  both. Saved client-arrival
  estimates from older builds stay hidden, and stored chats and
  token/cache/cost totals are unchanged. Keep bounded Claude request tracking
  and cached footer statistics. Remote use requires an updated Ubuntu engine
  as well as the Windows app.
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
- On older hosts, refresh account usage every five minutes in active and background
  windows, matching upstream's normal interval. Completed turns also trigger
  a refresh, deferred when necessary to honor the 30-second cooldown.
  Automatic refreshes probe only the current harness's active accounts;
  opening the account picker still refreshes all saved accounts. Remote scoped
  polling requires the
  patched Ubuntu engine; provider backoff and last-good usage caches remain.
- Give each saved account a stable, unique UI row ID so different providers
  cannot consume each other's Switch clicks. Retargeting Accounts clears old
  rows, menus, and pending UI tasks. Disconnected switches show an error;
  account actions cannot be overwritten by an earlier list refresh.
- Opt into **Auto-switch account** in the expanded Codex or Claude Code
  provider settings, separately for each device. Before a new idle turn,
  switch an exhausted session/weekly OAuth login to the saved account with
  the most remaining quota. Require recent, successful usage for both logins.
  Honor Codex's explicit workspace-credit and usage-limit gates even when
  session/weekly percentages are below 100%. Do not interpret an empty
  purchased-credit balance as an exhausted subscription. After a failed turn,
  recheck once after the usage-probe cooldown and rotate only if the host is
  idle; otherwise the next idle send checks again. Never replay that failure.
  Unknown, expired, and exhausted alternatives are skipped. Refresh inactive
  accounts only when the current account is exhausted, honoring existing
  provider cooldowns and backoff. Active turns, questions, voice, and background
  work prevent rotation. Retire only idle runtimes of the affected provider
  before replacing credentials, retaining native conversation IDs. Never
  replay a failed turn. Off by default; requires
  the updated engine as well as the client. API-key logins are not rotated.
- Choose **Circles** (default) or **Detailed** in **Settings > Appearance >
  Fonts and layout > Usage display**. Detailed mode keeps checkout/branch,
  TPS, context, and provider usage together on one footer line. Keep
  checkout/branch on the left and right-align TPS, Context, and Session/Weekly
  as one group. Context and Session/Weekly usage use matching segmented bars
  with percentages.
  Hover usage for reset countdowns; click any indicator for its full popover.
  Narrow layouts hide context counts first, then its bar and the TPS label;
  provider bars shorten while retaining percentages and warning colors.
  Missing data stays explicit. Countdown repaints do not increase polling.
- Restore a checkpoint's conversation, files, or both from a user message.
  Preserve the original thread; preview file changes and create a recovery
  backup before restoring. Busy runtimes, background tasks, and changed Git
  state prevent unsafe file restores. See [checkpoint restore](docs/checkpoint-restore.md)
  for limits and compatibility with older turns. The engine needs this feature
  as well as the client; upstream's temporary diff snapshots remain separate.
- Run Claude Code on an Anthropic-compatible API such as DeepSeek. Add an
  endpoint under **Settings > Providers > Claude Code > Custom endpoints**
  with a name, base URL, API key, and models; **Use DeepSeek** fills in
  everything except the key. The endpoint's models join the Claude Code model
  picker. Their runs start `claude` with the endpoint's base URL and key in
  the environment, and every model slot points at the endpoint, including
  background and subagent requests. Inherited Anthropic keys and provider
  switches are cleared for those runs only. Endpoints and keys are stored
  per device in the engine's `claude-endpoints.json` (0600), are never
  synced, and only a key's last four characters reach the UI. A chat keeps
  the backend it started on: once it exists, the picker offers only models
  from that backend, because providers sign their thinking blocks; start a
  new chat to switch. Endpoint runs leave account usage, auto-switch, and
  Claude's own cost reports aside, since the CLI would price them at
  Anthropic's rates. Token counts and TPS still work. The engine on the
  device running the agent needs this update.
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
2. Run `zeron-0.2.108-windows-x86_64-setup.exe` to replace the existing app in
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
