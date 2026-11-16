-- Settings → Usage's figures in a test host, which has no engine: two
-- agents over two periods for any range but 90 days, which has none. Each
-- range asked is pushed to `usage_stub_asked`; `usage_stub.fail_next` makes
-- the next read fail.

local usage = require("@account/usage")

local asked = ui.state("usage_stub_asked", {} :: { number })
local fail_next = ui.state("usage_stub_fail_next", false)

-- A promise already settled, as ui.call's would be.
local function settled(ok: boolean, value: any): any
	return {
		done = true,
		after = function(_self: any, on_ok: ((any) -> ())?, on_err: ((any) -> ())?)
			if ok and on_ok then
				on_ok(value)
			elseif not ok and on_err then
				on_err(value)
			end
		end,
	}
end

local EMPTY = {
	hourly = false, prices_loaded = true, sessions = 0, unpriced = 0,
	cost = "$0.000", tokens = "0", cost_n = 0, tokens_n = 0,
	fresh = "0", cache_read = "0", cache_write = "0", output = "0", saved = "$0.000", hit_rate = "0%",
	agents = {}, models = {}, periods = {},
}

local function sample(days: number): any
	return {
		hourly = days == 1, prices_loaded = true, sessions = 4, unpriced = 1,
		cost = "$18.40", tokens = "2.4M", cost_n = 18.4, tokens_n = 2400000,
		fresh = "210K", cache_read = "1.9M", cache_write = "120K", output = "170K", saved = "$5.13", hit_rate = "79%",
		agents = {
			{ harness = "claude-code", sessions = 3, cost = "$16.00", tokens = "1.9M", cost_n = 16, tokens_n = 1900000, cost_share = "87%", tokens_share = "79%" },
			{ harness = "codex", sessions = 1, cost = "$2.40", tokens = "500K", cost_n = 2.4, tokens_n = 500000, cost_share = "13%", tokens_share = "21%" },
		},
		models = {
			{ key = "claude-code/claude-sample", model = "claude-sample", harness = "claude-code", cost = "$16.00", tokens = "1.9M", cost_n = 16, tokens_n = 1900000, cost_share = "87%", tokens_share = "79%", hit_rate = "84%", per_million = "$8.42" },
			{ key = "codex/gpt-sample", model = "gpt-sample", harness = "codex", cost = "$2.40", tokens = "500K", cost_n = 2.4, tokens_n = 500000, cost_share = "13%", tokens_share = "21%", hit_rate = "61%", per_million = "$4.80" },
		},
		periods = {
			{ start = 1794700800, cost = "$0.000", tokens = "0", cost_n = 0, tokens_n = 0, split = {} },
			{ start = 1794787200, cost = "$18.40", tokens = "2.4M", cost_n = 18.4, tokens_n = 2400000, split = {
				{ harness = "claude-code", cost_n = 16, tokens_n = 1900000 },
				{ harness = "codex", cost_n = 2.4, tokens_n = 500000 },
			} },
		},
	}
end

usage.read = function(days: number): any
	asked:push(days)
	if fail_next:peek() then
		fail_next:set(false)
		return settled(false, { message = "failed" })
	end
	return settled(true, if days == 90 then EMPTY else sample(days))
end

ui.command {
	id = "usage_stub.fail_next",
	title = "The next usage read fails",
	run = function()
		fail_next:set(true)
	end,
}

return {}
