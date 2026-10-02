# Zeron Personal Windows Builds

This public fork of [zeronsh/zeron](https://github.com/zeronsh/zeron) keeps the
app named Zeron. The `patched-windows` branch starts from upstream `v0.2.101`
(`b42fc2b8fbf247dd92796c2917f277535cea91ac`). It is a personal build, not an
official Zeron release.

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
- Show structured asynchronous Codex questions in a separate panel with choices
  and custom answers, without replacing the normal chat draft. The panel uses
  the composer's theme-aware frosted-glass material. Questions stay answerable
  after the turn finishes; delivery errors preserve answers for retry. Remote
  chats require the patched engine on both devices. Installing this Windows
  build alone does not update the Ubuntu host.
- Show a persistent **Worked for** footer beneath completed assistant replies
  in normal mode, using the full stored turn duration, including tool work and
  waiting for input. Compact mode keeps its existing duration label; older
  turns without stored timing remain unchanged.
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
2. Run `zeron-0.2.101-windows-x86_64-setup.exe` to replace the existing app in
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

For each new upstream release, fetch its tag and merge it into the patched
branch. Merging preserves patch commits without rewriting history or requiring
a force push. For example, after upstream publishes `v0.2.102`:

```bash
git fetch upstream --tags
git switch patched-windows
git merge v0.2.102
# Resolve any conflicts while retaining the personal changes.
git push origin patched-windows
gh workflow run patched-windows.yml --repo trkshi/zeron --ref patched-windows
```

Do not use GitHub's **Sync fork** action to overwrite the patched branch.
Review upstream changes and any data migrations before installing a newer
build. Backups still matter when updating the upstream application itself.

Source changes and workflow logs are public. Never commit Zeron data,
credentials, personal avatars, or backups to this repository.
