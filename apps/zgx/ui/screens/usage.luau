-- The Usage page: a page of its own beside the chats (the rail's usage
-- ring opens it on Limits; ⌘K's "Usage" on Cost), as Zeron v2's dashboard
-- was. Three views over the device chosen, or all of your computers
-- together:
--
--   Cost, Tokens   what the coding agents used, read by each device's engine
--                  from the agents' own session logs (account/usage; the
--                  host adds the devices up): the total, the periods as bars
--                  stacked by agent, each device's and each agent's part,
--                  the kinds of token, then each model, which opens to its
--                  own chart and kinds. Costs are estimates at public API
--                  prices, not a plan's bill.
--   Limits         each agent's signed-in accounts and their plan windows
--                  (account/agents, as the rail's ring reads them), one row
--                  per account however many devices share its login, with
--                  the pool's unweighted average per window.
--
--   usage.view()        the page
--   usage.open(tab?)    to the page ("cost" | "tokens" | "limits"; else the
--                       one shown last)
--   usage.PLUGIN, usage.PAGE   the route it's shown on (a plugin page's,
--                       Zeron's own: screens/shell/route)

local ds = require("@ds")
local Chart = require("@ds/chart")
local Stat = require("@ds/stat")
local ink = require("@ds/visual_ink")
local locale = require("@ds/locale")
local PrivateText = require("@ds/private_text")
local usage = require("@account/usage")
local agents = require("@account/agents")
local sessions = require("@account/sessions")

local M = {}

M.PLUGIN = "zeron"
M.PAGE = "usage"

local CHART_H = 220
local MAX_W = 960

-- What's shown, kept across visits: the view, the device ("all", or a
-- device's id), the range, and the model opened (its key; nil: the list).
M.tab = ui.persist("usage_tab", "cost")
local device = ui.persist("usage_device", "all")
local range = ui.persist("usage_range", "30")
local opened = ui.state("usage_model_open", nil :: string?)

-- What came back for what was asked; the figures stay shown while another
-- range or device is read.
local figures = ui.state("usage_figures", nil :: any)
local reading = ui.state("usage_reading", false)
local failed = ui.state("usage_failed", "")
local asked_for = ""

function M.open(tab: string?)
	if tab then
		M.tab:set(tab)
	end
	local route = require("@app/screens/shell/route")
	route.plugin_page(M.PLUGIN, M.PAGE)
end

-- The agents Zeron drives (their names and marks), as the rail reads them.
local catalog = ui.shared("chat.catalog", { state = "looking", harnesses = {}, models = {} } :: any)
-- (Names and marks for agents the catalog doesn't list on this device: one
-- used only on another, as src/agents.rs `mark` names them.)
local KNOWN: { [string]: { label: string, mark: string } } = {
	["claude-code"] = { label = "Claude Code", mark = "claude" },
	codex = { label = "Codex", mark = "openai" },
	grok = { label = "Grok", mark = "grok" },
	cursor = { label = "Cursor", mark = "cursor" },
	opencode = { label = "OpenCode", mark = "opencode" },
	dsh = { label = "DeepSeek Harness", mark = "deepseek" },
}

local function agent(harness: string): any
	for _, h in catalog:get().harnesses or {} do
		if h.id == harness then
			return h
		end
	end
	local known = KNOWN[harness]
	return { id = harness, label = if known then known.label else harness, mark = if known then known.mark else nil }
end

local function mark(harness: string, size: number): Node
	local h = agent(harness)
	return ds.AgentMark.Mark { label = h.label, mark = h.mark, icon = h.icon, seed = harness, size = size, bare = true }
end

-- Your computers (this one first), and those of them to ask: the one
-- chosen, or every one online.
local function computers(): { any }
	return sessions.computers(sessions.devices:get(), true)
end

local function device_name(id: string): string
	for _, d in computers() do
		if d.id == id or (id == "" and d.here) then
			return d.name
		end
	end
	return id
end

-- The ids to ask ("" for this device), as a key that says the same ask.
local function scope(): { string }
	local chosen = device:get()
	local out: { string } = {}
	for _, d in computers() do
		local id = if d.here then "" else d.id
		if chosen == "all" then
			if d.here or d.online then
				table.insert(out, id)
			end
		elseif d.id == chosen then
			table.insert(out, id)
		end
	end
	if #out == 0 then
		-- (The device chosen is gone: this one.)
		table.insert(out, "")
	end
	return out
end

local function days(): number
	return tonumber(range:peek()) or 30
end

local function read()
	local ids = scope()
	local key = `{days()}|{table.concat(ids, ",")}`
	asked_for = key
	reading:set(true)
	failed:set("")
	usage.read(days(), ids):after(function(answer: any)
		if asked_for == key then
			figures:set(answer)
			reading:set(false)
		end
	end, function(err: any)
		if asked_for == key then
			failed:set(if type(err) == "table" and err.message then tostring(err.message) else tostring(err or "failed"))
			reading:set(false)
		end
	end)
end

local function in_cost(): boolean
	return M.tab:get() ~= "tokens"
end

-- A figure in the measure shown ("$12.34" or "1.5M").
local function shown(item: any): string
	return if in_cost() then item.cost else item.tokens
end

local function amount(item: any): number
	return if in_cost() then item.cost_n else item.tokens_n
end

local function percent_of(part: number, whole: number): number
	return if whole > 0 then math.clamp(part / whole, 0, 1) else 0
end

-- A period's name: "Nov 16", or "15:00" by the hour.
function M.period_name(start: number, hourly: boolean): string
	local t = os.date("*t", math.floor(start)) :: any
	if hourly then
		return string.format("%02d:00", t.hour)
	end
	return ui.t("time.month_day", { m = tostring(t.month), day = tostring(t.day) }):get()
end

-- A round top for the value axis and its ticks (about `steps` of 1, 2 or 5
-- times a power of ten).
function M.axis(peak: number, steps: number): (number, { number })
	if not (peak > 0) or peak == math.huge then
		return 1, { 0, 1 }
	end
	local rough = peak / steps
	local power = 10 ^ math.floor(math.log10(rough))
	local step = 10 * power
	for _, k in { 1, 2, 5 } do
		if k * power >= rough then
			step = k * power
			break
		end
	end
	local top = math.ceil(peak / step - 1e-9) * step
	local ticks = {}
	for v = 0, top + step / 2, step do
		table.insert(ticks, v)
	end
	return top, ticks
end

-- A tick's words: whole dollars where it can, else cents; tokens short.
function M.tick(v: number, dollars: boolean): string
	if dollars then
		return if v == math.floor(v) then `${math.floor(v)}` else string.format("$%.2f", v)
	end
	for _, unit in { { 1e9, "B" }, { 1e6, "M" }, { 1e3, "K" } } do
		if v >= unit[1] then
			local n = v / unit[1]
			return (if n == math.floor(n) then tostring(math.floor(n)) else string.format("%.1f", n)) .. unit[2]
		end
	end
	return tostring(math.floor(v))
end

local function axis_of(peak: number, dollars: boolean): any
	local top, ticks = M.axis(peak, 4)
	local labels = {}
	for i, t in ticks do
		labels[i] = M.tick(t, dollars)
	end
	return { lo = 0, hi = top, ticks = ticks, labels = labels }
end

-- The chart's data: a bar per period, stacked by agent, busiest at the
-- foot; or, for a model opened, its own bars.
local function chart_data(): any
	local f = figures:get()
	if not f then
		return nil
	end
	local dollars = in_cost()
	local x, series, peak = {}, {}, 0
	local model: any = nil
	local open = opened:get()
	for _, m in f.models do
		if m.key == open then
			model = m
		end
	end
	if model then
		local values = {}
		for p, period in f.periods do
			x[p] = M.period_name(period.start, f.hourly)
			local mp = model.periods and model.periods[p]
			local v = if mp then (if dollars then mp.cost_n else mp.tokens_n) else 0
			values[p] = v
			peak = math.max(peak, v)
		end
		series[1] = { name = model.model, color = "chart1", values = values }
	else
		for i, agent_row in f.agents do
			series[i] = { name = agent(agent_row.harness).label, color = `chart{i}`, values = {} }
		end
		for p, period in f.periods do
			x[p] = M.period_name(period.start, f.hourly)
			for i, agent_row in f.agents do
				local v = 0
				for _, part in period.split do
					if part.harness == agent_row.harness then
						v = if dollars then part.cost_n else part.tokens_n
					end
				end
				series[i].values[p] = v
			end
			peak = math.max(peak, amount(period))
		end
	end
	return {
		type = "bar",
		stacked = true,
		x = x,
		series = series,
		yaxis = axis_of(peak, dollars),
		fmt = if dollars then { prefix = "$", decimals = 2 } else { decimals = 0 },
	}
end

-- Pieces --------------------------------------------------------------------

local function number(text: string | () -> string): Node
	return ui.text { text, tabular = true, text_size = "$font.size.sm", color = "$text.primary", shrink = 0 }
end

local function section_title(title: any, trail: Node?): Node
	return ui.row {
		w_full = true,
		items = "center",
		gap = "$space.2",
		ui.text { title, weight = "$font.weight.medium", text_size = "$font.size.sm", color = "$text.primary", grow = 1 },
		trail,
	}
end

-- A card on the page: a quiet surface round a section.
local function card(props: { [number]: any, id: string? }): Node
	local p: any = {
		id = props.id,
		w_full = true,
		p = "$space.4",
		gap = "$space.3",
		rounded = "$radius.xl",
		bg = "$zeron.secondary.bg",
	}
	for i, child in props do
		if type(i) == "number" then
			table.insert(p, child)
		end
	end
	return ui.column(p)
end

-- A row's bar of its share, coloured.
local function share_bar(fraction: () -> number, color: () -> any): Node
	return ui.div {
		grow = 1,
		min_w = 24,
		h = 4,
		rounded = "$radius.full",
		bg = "$border.subtle",
		overflow = "hidden",
		ui.div {
			h_full = true,
			rounded = "$radius.full",
			bg = color,
			w = function(): string
				return `{math.floor(fraction() * 1000 + 0.5) / 10}%`
			end,
		},
	}
end

local function quiet(text: any): Node
	return ui.text { text, text_size = "$font.size.xs", color = "$text.tertiary" }
end

-- Cost and Tokens -----------------------------------------------------------

-- The head: the total in the measure, its sessions, and any calls left
-- unpriced or devices that didn't answer.
local function head(): Node
	return ui.column {
		w_full = true,
		gap = "$zeron.space.hair",
		ui.text {
			function(): string
				local f = figures:get()
				if f then
					return shown(f)
				end
				return if failed:get() ~= "" then ui.t("settings.usage.failed"):get() else ui.t("settings.usage.reading"):get()
			end,
			id = "usage-total",
			tabular = true,
			text_size = function(): string
				return if figures:get() then "$font.size.xxl" else "$font.size.lg"
			end,
			line_height = function(): string
				return if figures:get() then "$font.line.xxl" else "$font.line.lg"
			end,
			weight = "$font.weight.semibold",
			color = "$text.primary",
		},
		ui.text {
			function(): string
				local f = figures:get()
				if not f then
					return ""
				end
				local sessions_said = ui.t("settings.usage.sessions", { n = f.sessions }):get()
				if in_cost() then
					return `{ui.t("settings.usage.estimated", { sessions = sessions_said }):get()} · {ui.t("usage.saved", { amount = f.saved }):get()}`
				end
				return `{sessions_said} · {ui.t("settings.usage.hit_rate", { rate = f.hit_rate }):get()}`
			end,
			text_size = "$font.size.sm",
			color = "$text.secondary",
		},
		ui.show(function(): boolean
			local f = figures:get()
			return f ~= nil and in_cost() and (f.unpriced > 0 or not f.prices_loaded)
		end, quiet(function(): string
			local f = figures:get()
			if f and not f.prices_loaded then
				return ui.t("settings.usage.no_prices"):get()
			end
			return ui.t("settings.usage.unpriced", { n = if f then f.unpriced else 0 }):get()
		end)),
	}
end

-- Each device's part, while all of them are shown.
local function by_device(): Node
	local list = ui.computed(function(): { any }
		local f = figures:get()
		return if f and f.devices and device:get() == "all" then f.devices else {}
	end)
	return ui.show(
		function(): boolean
			return #list:get() > 1
		end,
		card {
			id = "usage-devices",
			section_title(ui.t("usage.devices.title")),
			ui.each(list, function(_d: any, _i: number, now: State<any>): Node
				local function share(): number
					local f = figures:get()
					return percent_of(amount(now:get()), if f then amount(f) else 0)
				end
				return ui.row {
					w_full = true,
					gap = "$space.3",
					items = "center",
					ds.Icon { name = "laptop", size = 14, color = "$text.secondary" },
					ui.text {
						function(): string
							return device_name(now:get().id)
						end,
						literal = true,
						truncate = true,
						w = 168,
						shrink = 0,
						text_size = "$font.size.sm",
						color = "$text.primary",
					},
					share_bar(share, function(): any
						return ink.paint("chart1")
					end),
					ui.text {
						function(): string
							local d = now:get()
							if d.failed then
								return ui.t(if d.failed == "offline" then "usage.devices.offline" else "usage.devices.failed"):get()
							end
							return shown(d)
						end,
						tabular = true,
						shrink = 0,
						text_size = "$font.size.sm",
						color = function(): string
							return if now:get().failed then "$text.tertiary" else "$text.primary"
						end,
					},
				}
			end, {
				key = function(d: any): string
					return d.id
				end,
				w_full = true,
				gap = "$space.3",
			}),
		}
	)
end

-- Each agent's part: its mark and name, its sessions, a bar of its share,
-- its figure.
local function by_agent(): Node
	local list = ui.computed(function(): { any }
		local f = figures:get()
		return if f then f.agents else {}
	end)
	return ui.each(list, function(_a: any, i: number, now: State<any>): Node
		local function share(): number
			local f = figures:get()
			return percent_of(amount(now:get()), if f then amount(f) else 0)
		end
		return ui.row {
			w_full = true,
			gap = "$space.3",
			items = "center",
			ui.div { w = 8, h = 8, shrink = 0, rounded = "$radius.full", bg = function(): any
				return ink.paint(`chart{i}`)
			end },
			ui.presence(function(): string
				return now:get().harness
			end, function(id: string): Node
				return mark(id, 16)
			end),
			ui.column {
				w = 168,
				shrink = 0,
				ui.text {
					function(): string
						return agent(now:get().harness).label
					end,
					literal = true,
					truncate = true,
					text_size = "$font.size.sm",
					color = "$text.primary",
				},
				quiet(function(): string
					return ui.t("settings.usage.sessions", { n = now:get().sessions }):get()
				end),
			},
			share_bar(share, function(): any
				return ink.paint(`chart{i}`)
			end),
			ui.text {
				function(): string
					local a = now:get()
					return if in_cost() then a.cost_share else a.tokens_share
				end,
				tabular = true,
				shrink = 0,
				w = 44,
				align = "right",
				text_size = "$font.size.xs",
				color = "$text.tertiary",
			},
			number(function(): string
				return shown(now:get())
			end),
		}
	end, {
		key = function(a: any): string
			return a.harness
		end,
		id = "usage-agents",
		w_full = true,
		gap = "$space.3",
	})
end

-- The kinds of token of `source` (the whole, or a model opened): a bar each
-- of its share of all of them.
local KINDS = { "fresh", "cache_read", "cache_write", "output" }
local function kinds(source: () -> any): Node
	local rows = {}
	for i, kind in KINDS do
		local function n(): number
			local s = source()
			return if s then s[`{kind}_n`] or 0 else 0
		end
		local function share(): number
			local s = source()
			if not s then
				return 0
			end
			local whole = 0
			for _, k in KINDS do
				whole += s[`{k}_n`] or 0
			end
			return percent_of(n(), whole)
		end
		table.insert(rows, ui.row {
			w_full = true,
			gap = "$space.3",
			items = "center",
			ui.column {
				w = 280,
				shrink = 0,
				ui.text { ui.t(`settings.usage.tokens.{kind}`), text_size = "$font.size.sm", color = "$text.primary" },
				ui.text { ui.t(`settings.usage.tokens.{kind}_about`), truncate = true, text_size = "$font.size.xs", color = "$text.tertiary" },
			},
			share_bar(share, function(): any
				return ink.paint(`chart{i}`)
			end),
			ui.text {
				function(): string
					return `{math.floor(share() * 100 + 0.5)}%`
				end,
				tabular = true,
				shrink = 0,
				w = 44,
				align = "right",
				text_size = "$font.size.xs",
				color = "$text.tertiary",
			},
			number(function(): string
				local s = source()
				return if s and s[kind] then s[kind] else "–"
			end),
		})
	end
	return ui.column { w_full = true, gap = "$space.3", table.unpack(rows) }
end

-- Each model, the priciest first: a row that opens its own view.
local function models(): Node
	local list = ui.computed(function(): { any }
		local f = figures:get()
		return if f then f.models else {}
	end)
	return ui.column {
		w_full = true,
		gap = "$space.1",
		ui.show(function(): boolean
			return figures:get() ~= nil and #list:get() == 0
		end, ds.EmptyState { icon = "clock-circle", title = ui.t("settings.usage.empty"), size = "sm" }),
		ui.each(list, function(_m: any, _i: number, now: State<any>): Node
			return ui.button {
				w_full = true,
				px = "$space.2",
				py = "$space.2",
				gap = "$space.3",
				items = "center",
				rounded = "$radius.lg",
				bg = "#00000000",
				hover = { bg = "$hover" },
				active = { bg = "$pressed" },
				cursor = "pointer",
				label = function(): string
					return now:get().model
				end,
				on_click = function()
					opened:set(now:peek().key)
				end,
				ui.presence(function(): string
					return now:get().harness
				end, function(id: string): Node
					return mark(id, 16)
				end),
				ui.column {
					grow = 1,
					min_w = 0,
					ui.text {
						function(): string
							return now:get().model
						end,
						literal = true,
						truncate = true,
						text_size = "$font.size.sm",
						color = "$text.primary",
					},
					ui.text {
						function(): string
							local m = now:get()
							local parts = { agent(m.harness).label, ui.t("settings.usage.hit_rate", { rate = m.hit_rate }):get() }
							if m.per_million ~= "" then
								table.insert(parts, ui.t("settings.usage.per_million", { price = m.per_million }):get())
							end
							return table.concat(parts, " · ")
						end,
						truncate = true,
						text_size = "$font.size.xs",
						color = "$text.tertiary",
					},
				},
				ui.text {
					function(): string
						local m = now:get()
						return if in_cost() then m.cost_share else m.tokens_share
					end,
					tabular = true,
					shrink = 0,
					text_size = "$font.size.xs",
					color = "$text.tertiary",
				},
				number(function(): string
					return shown(now:get())
				end),
				ds.Icon { name = "alt-arrow-right", size = 12, color = "$text.tertiary" },
			}
		end, {
			key = function(m: any): string
				return m.key
			end,
			w_full = true,
			gap = "$zeron.space.hair",
		}),
	}
end

local function the_chart(): Node
	return ui.div {
		id = "usage-chart",
		w_full = true,
		opacity = function(): number
			return if reading:get() then 0.6 else 1
		end,
		Chart { data = chart_data, height = CHART_H, label = locale.s("settings.usage.chart_title") },
	}
end

-- The model opened: back to the list, its name, its figures, its chart and
-- its kinds of token.
local function model_view(): Node
	local function model(): any
		local f, key = figures:get(), opened:get()
		for _, m in (if f then f.models else {}) do
			if m.key == key then
				return m
			end
		end
		return nil
	end
	return ui.column {
		w_full = true,
		gap = "$space.4",
		ui.row {
			gap = "$space.2",
			items = "center",
			ds.IconButton {
				icon = "alt-arrow-left",
				label = ui.t("usage.models.back"),
				on_click = function()
					opened:set(nil)
				end,
			},
			ui.presence(function(): string
				local m = model()
				return if m then m.harness else ""
			end, function(id: string): Node
				return if id ~= "" then mark(id, 18) else ui.div {}
			end),
			ui.text {
				function(): string
					local m = model()
					return if m then m.model else ""
				end,
				literal = true,
				weight = "$font.weight.semibold",
				text_size = "$font.size.lg",
				color = "$text.primary",
			},
		},
		card {
			ui.row {
				w_full = true,
				gap = "$space.6",
				items = "end",
				Stat {
					value = function(): string
						local m = model()
						return if m then shown(m) else "–"
					end,
					label = function(): string
						local m = model()
						return if m then ui.t("usage.models.share", { share = if in_cost() then m.cost_share else m.tokens_share }):get() else ""
					end,
				},
				Stat {
					value = function(): string
						local m = model()
						return if m then (if in_cost() then m.tokens else m.cost) else "–"
					end,
					label = function(): string
						return (if in_cost() then ui.t("settings.usage.tokens_word") else ui.t("settings.usage.cost")):get()
					end,
					size = "$font.size.lg",
					line = "$font.line.lg",
				},
				Stat {
					value = function(): string
						local m = model()
						return if m then m.hit_rate else "–"
					end,
					label = function(): string
						return ui.t("settings.usage.tokens.hit_rate"):get()
					end,
					size = "$font.size.lg",
					line = "$font.line.lg",
				},
				Stat {
					value = function(): string
						local m = model()
						return if m and m.saved then m.saved else "–"
					end,
					label = function(): string
						return ui.t("settings.usage.tokens.saved"):get()
					end,
					size = "$font.size.lg",
					line = "$font.line.lg",
				},
				Stat {
					value = function(): string
						local m = model()
						return if m and m.per_million ~= "" then m.per_million else "–"
					end,
					label = function(): string
						return ui.t("usage.models.per_million"):get()
					end,
					size = "$font.size.lg",
					line = "$font.line.lg",
				},
			},
			the_chart(),
		},
		card {
			section_title(ui.t("usage.kinds.title")),
			kinds(model),
		},
	}
end

local function spend_view(): Node
	return ui.switch(function(): string
		return if opened:get() then "model" else "all"
	end, {
		model = model_view(),
		all = ui.column {
			w_full = true,
			gap = "$space.4",
			card {
				head(),
				ui.show(function(): boolean
					return figures:get() ~= nil
				end, the_chart()),
				by_agent(),
			},
			by_device(),
			card {
				section_title(ui.t("usage.kinds.title"), quiet(function(): string
					local f = figures:get()
					return if f then ui.t("settings.usage.hit_rate", { rate = f.hit_rate }):get() else ""
				end)),
				kinds(function(): any
					return figures:get()
				end),
			},
			card {
				id = "usage-models",
				section_title(ui.t("usage.models.title")),
				models(),
			},
		},
	})
end

-- Limits ----------------------------------------------------------------------

-- The accounts' keys (account/agents `on_device`) of the devices shown: ""
-- for this one, else the device's id.
local function account_keys(): { string }
	local chosen = device:get()
	local out: { string } = {}
	for _, d in computers() do
		if chosen == "all" and (d.here or d.online) or d.id == chosen then
			table.insert(out, if d.here then "" else d.id)
		end
	end
	if #out == 0 then
		table.insert(out, "")
	end
	return out
end

local function window_name(label: string): string
	local key = string.lower(label or "")
	if key == "session" or key == "week" or key == "day" then
		return locale.s(`shell.rail.usage.window.{key}`)
	end
	return label
end

local function percent(used: number): string
	return `{math.floor(math.clamp(used, 0, 1) * 100 + 0.5)}%`
end

-- The countdowns' clock, a minute at a time.
local clock = ui.state("usage_clock", os.time())
ui.every(30000, function()
	clock:set(os.time())
end)

local function until_words(at: number): string
	local left = math.max(0, at - math.max(clock:get(), os.time()))
	local d, h, m = left // 86400, (left % 86400) // 3600, (left % 3600) // 60
	if d > 0 then
		return locale.s("shell.rail.usage.span.days", { d = d, h = h })
	elseif h > 0 then
		return locale.s("shell.rail.usage.span.hours", { h = h, m = m })
	elseif m > 0 then
		return locale.s("shell.rail.usage.span.minutes", { m = m })
	end
	return locale.s("shell.rail.usage.span.soon")
end

-- Every agent's accounts on the devices shown, one per login (the same
-- login copied to several devices is one account: the reading without an
-- error kept, and the devices it's on), each agent's in catalog order.
type Pooled = { harness: string, accounts: { any } }
local function pooled(): { Pooled }
	local on = agents.on_device:get()
	local by: { [string]: { [string]: any } } = {}
	local order: { [string]: { string } } = {}
	for _, key in account_keys() do
		for harness, list in on[key] or {} do
			by[harness] = by[harness] or {}
			order[harness] = order[harness] or {}
			for _, account in list do
				if account.windows and (#account.windows > 0 or account.error) then
					local id = if account.email ~= "" then account.email else account.id
					local known = by[harness][id]
					local where = device_name(key)
					if not known then
						local copy = table.clone(account)
						copy.where = { where }
						by[harness][id] = copy
						table.insert(order[harness], id)
					else
						table.insert(known.where, where)
						known.active = known.active or account.active
						if known.error and not account.error then
							local keep = known.where
							local copy = table.clone(account)
							copy.where = keep
							copy.active = known.active
							by[harness][id] = copy
						end
					end
				end
			end
		end
	end
	local out: { Pooled } = {}
	local seen: { [string]: boolean } = {}
	local function add(harness: string)
		if seen[harness] or not order[harness] or #order[harness] == 0 then
			return
		end
		seen[harness] = true
		local accounts = {}
		for _, id in order[harness] do
			table.insert(accounts, by[harness][id])
		end
		-- (In use first, then the fullest.)
		table.sort(accounts, function(a: any, b: any): boolean
			if a.active ~= b.active then
				return a.active
			end
			local function most(x: any): number
				local m = 0
				for _, w in x.windows or {} do
					m = math.max(m, w.used)
				end
				return m
			end
			return most(a) > most(b)
		end)
		table.insert(out, { harness = harness, accounts = accounts })
	end
	for _, h in catalog:get().harnesses or {} do
		add(h.id)
	end
	for harness in order do
		add(harness)
	end
	return out
end

-- The pool's unweighted average of each window across its accounts that
-- report it: { { label, used } }, in the first account's order.
local function pool_average(accounts: { any }): { any }
	local sums: { [string]: { n: number, used: number } } = {}
	local labels: { string } = {}
	for _, a in accounts do
		if not a.error then
			for _, w in a.windows or {} do
				if not sums[w.label] then
					sums[w.label] = { n = 0, used = 0 }
					table.insert(labels, w.label)
				end
				sums[w.label].n += 1
				sums[w.label].used += math.clamp(w.used, 0, 1)
			end
		end
	end
	local out = {}
	for _, label in labels do
		table.insert(out, { label = label, used = sums[label].used / sums[label].n })
	end
	return out
end

local function tone_of(used: number): string
	return if used >= 0.95 then "danger" elseif used >= 0.8 then "warning" else "accent"
end

-- The accounts as they stand, by agent and by account (`id`: its email,
-- else its id), each agent's pool average by window: read by the rows,
-- each keyed, so a refresh moves their bars and figures in place rather
-- than building them again (as the rail's card does).
type Pool = { accounts: { string }, by_id: { [string]: any }, average: { string }, average_of: { [string]: number } }
type Board = { order: { string }, pools: { [string]: Pool } }

local function board_of(list: { Pooled }): Board
	local order: { string } = {}
	local pools: { [string]: Pool } = {}
	for _, p in list do
		table.insert(order, p.harness)
		local ids: { string } = {}
		local by_id: { [string]: any } = {}
		for _, a in p.accounts do
			local id = if a.email ~= "" then a.email else a.id
			table.insert(ids, id)
			by_id[id] = a
		end
		local average: { string } = {}
		local average_of: { [string]: number } = {}
		for _, w in pool_average(p.accounts) do
			table.insert(average, w.label)
			average_of[w.label] = w.used
		end
		pools[p.harness] = { accounts = ids, by_id = by_id, average = average, average_of = average_of }
	end
	return { order = order, pools = pools }
end

-- A window: its name, its bar, its share, and when it starts over, each
-- read from `get` as it changes (nil: gone, drawn empty until its row goes).
local function window_row(label: string, get: () -> any): Node
	local function used(): number
		local w = get()
		return if w then math.clamp(w.used, 0, 1) else 0
	end
	local function hot(): boolean
		return used() >= 1
	end
	return ui.row {
		w_full = true,
		items = "center",
		gap = "$space.3",
		ui.text {
			window_name(label),
			w = 64,
			shrink = 0,
			truncate = true,
			text_size = "$font.size.sm",
			color = function(): string
				return if hot() then "$danger" else "$text.secondary"
			end,
		},
		ds.ProgressBar {
			value = used,
			tone = function(): any
				return tone_of(used())
			end,
			grow = 1,
		},
		ui.text {
			function(): string
				return percent(used())
			end,
			w = 44,
			shrink = 0,
			tabular = true,
			align = "right",
			text_size = "$font.size.sm",
			color = function(): string
				return if hot() then "$danger" else "$text.primary"
			end,
		},
		ui.text {
			function(): string
				local w = get()
				local resets = if w then w.resets else nil
				if not resets then
					return if hot() and w and w.label then locale.s("shell.rail.usage.limit_reached") else ""
				end
				return locale.s(if hot() then "shell.rail.usage.limit_resets_in" else "shell.rail.usage.resets_in", { when = until_words(resets) })
			end,
			w = 200,
			shrink = 0,
			tabular = true,
			truncate = true,
			text_size = "$font.size.xs",
			color = function(): string
				return if hot() then "$danger" else "$text.tertiary"
			end,
		},
	}
end

local function account_row(board: State<Board>, harness: string, id: string): Node
	local function account(): any
		local pool = board:get().pools[harness]
		return if pool then pool.by_id[id] or {} else {}
	end
	local function window(label: string): () -> any
		return function(): any
			for _, w in account().windows or {} do
				if w.label == label then
					return w
				end
			end
			return nil
		end
	end
	return ui.column {
		w_full = true,
		gap = "$space.2",
		-- (A hairline above every account but the first.)
		ui.show(function(): boolean
			local pool = board:get().pools[harness]
			return pool ~= nil and pool.accounts[1] ~= id
		end, ui.div { w_full = true, h = 1, bg = "$border.subtle" }),
		ui.column {
			w_full = true,
			gap = "$space.2",
			py = "$space.2",
			ui.row {
				w_full = true,
				items = "center",
				gap = "$space.2",
				PrivateText { id, reveal = false, truncate = true, shrink = 1, min_w = 0, text_size = "$font.size.sm", color = "$text.primary" },
				ui.show(function(): boolean
					return (account().plan or "") ~= ""
				end, ui.text {
					function(): string
						return account().plan or ""
					end,
					shrink = 0,
					text_size = "$font.size.xs",
					color = "$text.secondary",
				}),
				ui.show(function(): boolean
					return account().active == true
				end, ds.Badge { ui.t("usage.limits.active"), tone = "accent" :: any, size = "sm" :: any, dot = true }),
				ui.div { grow = 1 },
				ui.show(function(): boolean
					return device:get() == "all"
				end, quiet(function(): string
					return table.concat(account().where or {}, ", ")
				end)),
			},
			ui.show(function(): boolean
				return account().error ~= nil
			end, ui.text {
				function(): string
					return account().error or ""
				end,
				wrap = true,
				text_size = "$font.size.xs",
				color = "$warning",
			}),
			ui.each(ui.computed(function(): { string }
				local labels: { string } = {}
				for _, w in account().windows or {} do
					table.insert(labels, w.label)
				end
				return labels
			end), function(label: string): Node
				return window_row(label, window(label))
			end, {
				key = function(label: string): string
					return label
				end,
				w_full = true,
				gap = "$space.2",
			}),
		},
	}
end

local function agent_pool(board: State<Board>, harness: string): Node
	local function pool(): Pool
		return board:get().pools[harness] or { accounts = {}, by_id = {}, average = {}, average_of = {} }
	end
	local h = agent(harness)
	return card {
		ui.row {
			w_full = true,
			items = "center",
			gap = "$space.2",
			mark(harness, 18),
			ui.text { h.label, literal = true, weight = "$font.weight.medium", text_size = "$font.size.md", color = "$text.primary" },
			quiet(function(): string
				return ui.t("usage.limits.accounts", { n = #pool().accounts }):get()
			end),
		},
		ui.show(function(): boolean
			local p = pool()
			return #p.accounts > 1 and #p.average > 0
		end, ui.column {
			w_full = true,
			gap = "$space.2",
			p = "$space.3",
			rounded = "$radius.lg",
			bg = "$hover",
			ui.row {
				gap = "$space.2",
				items = "baseline",
				ui.text { ui.t("usage.limits.pool"), weight = "$font.weight.medium", text_size = "$font.size.sm", color = "$text.primary" },
				quiet(ui.t("usage.limits.pool_about")),
			},
			ui.each(ui.computed(function(): { string }
				return pool().average
			end), function(label: string): Node
				return window_row(label, function(): any
					local used = pool().average_of[label]
					return if used then { label = label, used = used } else nil
				end)
			end, {
				key = function(label: string): string
					return label
				end,
				w_full = true,
				gap = "$space.2",
			}),
		}),
		ui.each(ui.computed(function(): { string }
			return pool().accounts
		end), function(id: string): Node
			return account_row(board, harness, id)
		end, {
			key = function(id: string): string
				return id
			end,
			w_full = true,
			gap = 0,
		}),
	}
end

local function limits_view(): Node
	-- Each device shown is watched (and asked again as the page opens).
	ui.effect(function()
		for _, key in account_keys() do
			agents.watch_device(key)
		end
	end)
	local board = ui.computed(function(): Board
		return board_of(pooled())
	end)
	local order = ui.computed(function(): { string }
		return board:get().order
	end)
	return ui.column {
		w_full = true,
		gap = "$space.4",
		ui.show(function(): boolean
			return #order:get() == 0
		end, card {
			ds.EmptyState {
				icon = "chart-square",
				size = "sm",
				title = function(): string
					local on = agents.on_device:get()
					for _, key in account_keys() do
						if on[key] == nil then
							return ui.t("usage.limits.loading"):get()
						end
					end
					return ui.t("usage.limits.none"):get()
				end,
			},
		}),
		ui.each(order, function(harness: string): Node
			return agent_pool(board, harness)
		end, {
			key = function(harness: string): string
				return harness
			end,
			w_full = true,
			gap = "$space.4",
		}),
	}
end

-- The page -----------------------------------------------------------------

local function device_items(): { any }
	local items = { { value = "all", label = ui.t("usage.device.all"):get(), icon = "monitor" } }
	for _, d in computers() do
		table.insert(items, {
			value = d.id,
			label = d.name,
			icon = "laptop",
			description = if d.here then ui.t("usage.device.here"):get() elseif not d.online then ui.t("usage.devices.offline"):get() else nil,
		})
	end
	return items
end

local function header(): Node
	return ui.row {
		w_full = true,
		items = "center",
		gap = "$space.3",
		wrap = true,
		ui.column {
			grow = 1,
			min_w = 200,
			gap = "$zeron.space.hair",
			ui.text { ui.t("usage.title"), weight = "$font.weight.semibold", text_size = "$font.size.xl", color = "$text.primary" },
			ui.text {
				function(): string
					return if M.tab:get() == "limits" then ui.t("usage.limits.description"):get() else ui.t("usage.description"):get()
				end,
				wrap = true,
				text_size = "$font.size.sm",
				color = "$text.secondary",
			},
		},
		ui.row {
			gap = "$space.2",
			items = "center",
			shrink = 0,
			ds.Dropdown {
				value = device :: any,
				items = device_items,
				icon = function(): any
					return if device:get() == "all" then "monitor" else "laptop"
				end,
				search = false,
			},
			ui.show(function(): boolean
				return M.tab:get() ~= "limits"
			end, ds.Segmented {
				value = range,
				id = "usage-range",
				label = ui.t("settings.usage.range"),
				options = {
					{ value = "1", label = ui.t("settings.usage.range_1") },
					{ value = "7", label = ui.t("settings.usage.range_7") },
					{ value = "30", label = ui.t("settings.usage.range_30") },
					{ value = "90", label = ui.t("settings.usage.range_90") },
				} :: any,
			}),
			ui.div {
				opacity = function(): number
					return if reading:get() then 0.5 else 1
				end,
				ds.IconButton {
					icon = "refresh",
					label = ui.t("settings.usage.refresh"),
					on_click = function()
						if M.tab:peek() == "limits" then
							for _, key in account_keys() do
								agents.refresh_device(key)
							end
						else
							read()
						end
					end,
				},
			},
		},
	}
end

function M.view(): Node
	-- Read as the page opens and whenever the range or the devices asked
	-- change (a device coming online, while all are shown).
	local ask = ui.computed(function(): string
		return `{range:get()}|{table.concat(scope(), ",")}`
	end)
	ui.effect(function()
		ask:get()
		read()
	end)
	-- (A model opened belongs to the figures it came from.)
	ui.effect(function()
		device:get()
		opened:set(nil)
	end)
	return ui.scroll {
		id = "usage-page",
		w_full = true,
		h_full = true,
		fade = { 12, 12 },
		ui.column {
			w_full = true,
			items = "center",
			px = "$space.6",
			py = "$space.5",
			ui.column {
				w_full = true,
				max_w = MAX_W,
				gap = "$space.4",
				header(),
				ds.Tabs {
					value = M.tab,
					label = ui.t("usage.title"),
					options = {
						{ value = "cost", label = ui.t("usage.tab.cost"), icon = "wallet" },
						{ value = "tokens", label = ui.t("usage.tab.tokens"), icon = "chart-square" },
						{ value = "limits", label = ui.t("usage.tab.limits"), icon = "clock-circle" },
					} :: any,
				},
				ui.switch(function(): string
					return if M.tab:get() == "limits" then "limits" else "spend"
				end, {
					spend = spend_view(),
					limits = limits_view(),
				}),
			},
		},
	}
end

return M
