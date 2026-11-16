-- What this device's coding agents spent, read by the engine from their own
-- session logs and added up by the host (src/usage.rs).
--
--   usage.read(days)   1, 7, 30 or 90: a promise of the figures (their shape
--                      is at src/usage.rs), rejecting with an outcome word
--
-- A field, so a test host answers in the engine's place (usage_stub.luau).

local M = {}

function M.read(days: number): any
	return ui.call("usage.read", { days = days })
end

return M
