# Usage History

Open **Usage** with the speedometer button beside Pull requests and Settings
in the sidebar footer. Select the device that runs your agents, then choose
**Tokens**, **Cost**, or **Limits**. The page starts in Tokens with a 30-day
period. It is also available as **Open usage dashboard** in the Ctrl+K command
palette. Escape closes a detail view, then returns to the previous page.

## Tokens and Cost

The 24-hour view uses hourly buckets. The 7-, 30-, and 90-day views use calendar
days in your computer's local time zone. Provider summaries and model/day
breakdowns share the same date filter. Hover a trend for its period total;
select a model row for its own chart and billing categories.

Input includes cache reads and writes. Output includes reasoning, so those
categories must not be added to input/output totals again. Missing reports
remain unknown; partially reported categories show known subtotals with a
partial-data notice. Codex has no separately billed cache-write category,
which is normalized to zero in dashboard aggregates, not in saved messages.

Provider-reported USD costs take precedence over estimates. For other turns,
the dashboard calculates standard API value only when the model has known
rates and its input/output/cache billing counts are complete. Estimates and
API cache savings are **not subscription bills, actual savings, or refunds**.
Reasoning is already included in the output price.
When a provider reports only a total cost, category shares are allocated in
proportion to standard API rates, not claimed to be reported category charges.

Model rates come from LiteLLM's public model pricing catalog on GitHub. Prices
are cached on the host for 24 hours. Manual refresh requests a new catalog,
subject to a one-minute cooldown. Failed downloads preserve the last good
catalog. Exact model identifiers are preferred; ambiguous short aliases are
not guessed. Unknown models and incomplete billing counts stay unpriced.
Mixed totals exclude unpriced turns from the dollar amount.

## Limits

Codex and Claude OAuth accounts use Zeron's existing account-usage cache.
Session and weekly rows show equal account segments, remaining percentages,
reset times, and account details. The pool percentage is an **unweighted
account average**, not combined capacity: different subscription plans can
have different allowances. Stale, expired, offline, and failed readings remain
labelled. This dashboard does not switch accounts or change auto-switch settings.

## Quota Refreshes

The host engine schedules quota refreshes for every running thread, independently
of which conversation is selected or whether a client is connected. Completed
tool calls (including edits and shell commands), changed token reports, and the
transition from streamed reasoning to text or a tool call request a refresh.
Raw reasoning/text chunks and tool-output chunks do not each send a request.
Finishing, stopping, or failing a turn requests a final refresh too.

Activity for the same provider is coalesced into one pending job; provider probes
remain limited to one attempt per account per 30 seconds. Events arriving during
the cooldown join the pending job without postponing it, and events arriving
during a probe get one trailing refresh. Quiet turns and background subagents
retain a five-minute fallback. Connected clients also keep active-account
readings fresh on that fallback; an idle host without clients does not poll.

Automatic refreshes probe active logins only. Saved inactive accounts retain their
cached readings until an explicit account/pool refresh, opening the footer account
popover or expanding a Home provider group, or an auto-switch check needs
alternatives. The host preserves the last successful reading on failure and
honors provider backoff and Retry-After, including manual refreshes. A changed
login does not inherit another account's backoff. Redeeming
a reset credit retains its immediate post-redemption probe.

`WatchAgentAccounts` publishes quota snapshots to the selected host's UI cache,
shared by the Home account pool, composer footer, and Limits dashboard. Clients
on a host advertising `account-usage-refresh-v1` no longer run their own automatic
quota probes; older hosts retain the previous five-minute UI polling and
selected-chat completion refresh. Quota percentages still come from providers,
not from counting edits or tools, and providers may report them after a delay.

## Coverage and Storage

The first version covers usage recorded by Zeron, not standalone CLI history.
Each host maintains `usage-history.sqlite3` in its existing profile store.
It saves only message/chat/device IDs, timestamps, provider/model identifiers,
and usage scalars. No prompt text, tool output, credentials, or pricing requests
containing your conversations are stored or sent to the pricing source.

New completed, interrupted, and failed turns with reported usage are indexed
at their turn boundary. Repeated reports overwrite the same message ID; copied
fork history is not billed again. Restoring checkpoints or deleting a thread
does not remove usage already consumed and recorded in this index.

On opening the page, existing local-host snapshots are scanned for billing
scalars only. Older snapshots can contain only recent messages; missing history
cannot be reconstructed. Their model/provider are unknown because older message
records did not save those fields. Unavailable, malformed, or over-32-MiB
snapshots are skipped and disclosed. External CLI logs and synced conversations
hosted on other devices are not imported. Select another host to read its index.

The Cost and Tokens views refresh history at most every 30 seconds while visible
and the window is active. Limits uses streamed quota snapshots on supporting
hosts, falling back to cached reads on older hosts. Snapshot backfill is limited
to once every two minutes unless manually refreshed. It stops polling when
hidden, offline, or inactive. Index
writes run off the engine's async executor and never run on text deltas or UI
animation frames. A failed index does not prevent normal conversations.

Both the Windows UI and the engine on the selected device require the update.
The existing composer TPS, context indicators, account switching, and chat
storage are unchanged.
