-- What the coding agents spent, read by each device's engine from their own
-- session logs and added up by the host (src/usage.rs).
--
--   usage.read(days, devices?)   1, 7, 30 or 90 days, over `devices` (ids,
--                      "" for this one; none: this one alone): a promise of
--                      the figures (their shape is at src/usage.rs),
--                      rejecting with an outcome word when none answered
--
-- A field, so a test host answers in the engine's place (usage_stub.luau).

local M = {}

function M.read(days: number, devices: { string }?): any
	return ui.call("usage.read", { days = days, devices = devices })
end

return M
