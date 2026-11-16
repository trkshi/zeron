-- Settings → Usage: what the coding agents on this device used, from their
-- own session logs (account/usage: the engine reads them, the host adds them
-- up): over the past day, week, month or quarter, in dollars or tokens, the
-- periods as bars stacked by agent, the kinds of token and what the cache
-- saved, then each model (its cache hit rate and what a million of its
-- tokens came to) or each period. Costs are estimates at public API prices,
-- not a plan's bill. Plan limits are on the rail's usage ring and Providers.

local ds = require("@ds")
local Chart = require("@ds/chart")
local ink = require("@ds/visual_ink")
local usage = require("@account/usage")
local search = require("@app/screens/settings/search")

local M = {}

M.ID = "usage"
M.TITLE = "Usage"

search.index(M.ID, ui.t("settings.usage.title"), ui.t("settings.usage.terms"), {
	{ key = "usage_chart", title = ui.t("settings.usage.chart_title"), terms = ui.t("settings.usage.terms") },
	{ key = "usage_tokens", title = ui.t("settings.usage.tokens.title"), terms = ui.t("settings.usage.tokens.terms") },
	{ key = "usage_breakdown", title = ui.t("settings.usage.breakdown.title"), terms = ui.t("settings.usage.breakdown.terms") },
})

local CHART_H = 200

-- The agents Zeron drives (their names and marks), as the rail reads them.
local catalog = ui.shared("chat.catalog", { state = "looking", harnesses = {}, models = {} } :: any)
-- (Names for agents the catalog doesn't list on this device.)
local KNOWN: { [string]: string } = { ["claude-code"] = "Claude Code", codex = "Codex", grok = "Grok", dsh = "DeepSeek Harness" }

local function agent(harness: string): any
	for _, h in catalog:get().harnesses or {} do
		if h.id == harness then
			return h
		end
	end
	return { id = harness, label = KNOWN[harness] or harness, mark = if harness == "dsh" then "provider-deepseek" else nil }
end

-- What's asked and what came back. The range and the measure are kept
-- across visits; the figures stay shown while a new range is read.
local range = ui.persist("settings_usage_range", "30")
local measure = ui.persist("settings_usage_measure", "cost")
local grouping = ui.persist("settings_usage_by", "model")
local figures = ui.state("settings_usage_figures", nil :: any)
local reading = ui.state("settings_usage_reading", false)
local failed = ui.state("settings_usage_failed", "")

local function days(): number
	return tonumber(range:peek()) or 30
end

local function read()
	local asked = days()
	reading:set(true)
	failed:set("")
	usage.read(asked):after(function(answer: any)
		if days() == asked then
			figures:set(answer)
			reading:set(false)
		end
	end, function(err: any)
		if days() == asked then
			failed:set(if type(err) == "table" and err.message then tostring(err.message) else "failed")
			reading:set(false)
		end
	end)
end

local function in_cost(): boolean
	return measure:get() == "cost"
end

-- A figure in the measure shown ("$12.34" or "1.5M").
local function shown(item: any): string
	return if in_cost() then item.cost else item.tokens
end

local function amount(item: any): number
	return if in_cost() then item.cost_n else item.tokens_n
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

-- The chart's data: a bar per period, stacked by agent, busiest at the foot.
local function chart_data(): any
	local f = figures:get()
	if not f then
		return nil
	end
	local dollars = in_cost()
	local x, series = {}, {}
	for i, agent_row in f.agents do
		series[i] = { name = agent(agent_row.harness).label, color = `chart{i}`, values = {} }
	end
	local peak = 0
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
	local top, ticks = M.axis(peak, 4)
	local labels = {}
	for i, t in ticks do
		labels[i] = M.tick(t, dollars)
	end
	return {
		type = "bar",
		stacked = true,
		x = x,
		series = series,
		yaxis = { lo = 0, hi = top, ticks = ticks, labels = labels },
		fmt = if dollars then { prefix = "$", decimals = 2 } else { decimals = 0 },
	}
end

local function number(text: string | () -> string): Node
	return ui.text { text, tabular = true, text_size = "$font.size.sm", color = "$text.primary", shrink = 0 }
end

-- The head: the total in the measure, its sessions, and any calls left
-- unpriced; the measure's switch at its end.
local function head(): Node
	return ui.row {
		w_full = true,
		px = "$space.4",
		pt = "$space.4",
		gap = "$space.3",
		items = "start",
		ui.column {
			grow = 1,
			min_w = 0,
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
					local sessions = ui.t("settings.usage.sessions", { n = f.sessions }):get()
					return if in_cost() then ui.t("settings.usage.estimated", { sessions = sessions }):get() else sessions
				end,
				text_size = "$font.size.sm",
				color = "$text.secondary",
			},
			ui.show(function(): boolean
				local f = figures:get()
				return f ~= nil and in_cost() and (f.unpriced > 0 or not f.prices_loaded)
			end, ui.text {
				function(): string
					local f = figures:get()
					if f and not f.prices_loaded then
						return ui.t("settings.usage.no_prices"):get()
					end
					return ui.t("settings.usage.unpriced", { n = if f then f.unpriced else 0 }):get()
				end,
				text_size = "$font.size.sm",
				color = "$text.tertiary",
			}),
		},
		ds.Segmented {
			value = measure,
			label = ui.t("settings.usage.measure"),
			options = {
				{ value = "cost", label = ui.t("settings.usage.cost") },
				{ value = "tokens", label = ui.t("settings.usage.tokens_word") },
			} :: any,
		},
	}
end

-- Each agent's part: its mark and name, its sessions, a bar of its share,
-- its figure.
local function agents(): Node
	local list = ui.computed(function(): { any }
		local f = figures:get()
		return if f then f.agents else {}
	end)
	return ui.each(list, function(_a: any, i: number, now: State<any>): Node
		local function share(): number
			local f, a = figures:get(), now:get()
			local whole = if f then amount(f) else 0
			return if whole > 0 then amount(a) / whole else 0
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
				local h = agent(id)
				return ds.AgentMark.Mark { label = h.label, mark = h.mark, icon = h.icon, seed = id, size = 16, bare = true }
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
				ui.text {
					function(): string
						return ui.t("settings.usage.sessions", { n = now:get().sessions }):get()
					end,
					text_size = "$font.size.xs",
					color = "$text.tertiary",
				},
			},
			ui.div {
				grow = 1,
				min_w = 24,
				h = 4,
				rounded = "$radius.full",
				bg = "$zeron.secondary.bg",
				overflow = "hidden",
				ui.div {
					h_full = true,
					rounded = "$radius.full",
					bg = function(): any
						return ink.paint(`chart{i}`)
					end,
					w = function(): string
						return `{math.floor(share() * 1000 + 0.5) / 10}%`
					end,
				},
			},
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
		px = "$space.4",
		gap = "$space.3",
	})
end

local function overview(): Node
	return ds.Settings.Section {
		search.anchor(
			"usage_chart",
			ds.Settings.Group {
				ui.column {
					w_full = true,
					gap = "$space.4",
					pb = "$space.4",
					head(),
					ui.show(function(): boolean
						return figures:get() ~= nil
					end, ui.column {
						w_full = true,
						px = "$space.4",
						gap = "$space.4",
						ui.div {
							id = "usage-chart",
							w_full = true,
							opacity = function(): number
								return if reading:get() then 0.6 else 1
							end,
							Chart { data = chart_data, height = CHART_H, label = ui.t("settings.usage.chart_title"):get() },
						},
					}),
					agents(),
				},
			}
		),
	}
end

-- The kinds of token, what the cache read and what it saved.
local function tokens(): Node
	local function row(key: string, field: string, description: string?): Node
		return ds.Settings.Row {
			label = ui.t(`settings.usage.tokens.{key}`),
			description = if description then ui.t(`settings.usage.tokens.{description}`) else nil,
			control = number(function(): string
				local f = figures:get()
				return if f then f[field] else "–"
			end),
		}
	end
	return ds.Settings.Section {
		title = ui.t("settings.usage.tokens.title"),
		search.anchor(
			"usage_tokens",
			ds.Settings.Group {
				row("total", "tokens"),
				row("fresh", "fresh", "fresh_about"),
				row("cache_read", "cache_read", "cache_read_about"),
				row("cache_write", "cache_write", "cache_write_about"),
				row("output", "output", "output_about"),
				row("hit_rate", "hit_rate", "hit_rate_about"),
				row("saved", "saved", "saved_about"),
			}
		),
	}
end

-- Each model (the priciest first) or each period with any use (the latest
-- first).
local function breakdown(): Node
	local rows = ui.computed(function(): { any }
		local f = figures:get()
		if not f then
			return {}
		end
		if grouping:get() == "model" then
			return f.models
		end
		local out = {}
		for i = #f.periods, 1, -1 do
			local p = f.periods[i]
			if p.cost_n > 0 or p.tokens_n > 0 then
				table.insert(out, p)
			end
		end
		return out
	end)
	return ds.Settings.Section {
		title = ui.t("settings.usage.breakdown.title"),
		trail = ds.Segmented {
			value = grouping,
			label = ui.t("settings.usage.breakdown.title"),
			options = {
				{ value = "model", label = ui.t("settings.usage.breakdown.model") },
				{
					value = "period",
					label = function(): string
						local f = figures:get()
						return if f and f.hourly then ui.t("settings.usage.breakdown.hour"):get() else ui.t("settings.usage.breakdown.day"):get()
					end,
				},
			} :: any,
		},
		search.anchor(
			"usage_breakdown",
			ui.show(function(): boolean
				return figures:get() ~= nil and #rows:get() == 0
			end, ds.EmptyState { icon = "clock-circle", title = ui.t("settings.usage.empty"), size = "sm" })
		),
		ds.Settings.Each(rows, function(item: any, _i: number, now: State<any>): Node
			if item.model then
				return ds.Settings.Row {
					label = function(): string
						return now:get().model
					end,
					description = function(): string
						local m = now:get()
						local parts = { agent(m.harness).label, ui.t("settings.usage.hit_rate", { rate = m.hit_rate }):get() }
						if m.per_million ~= "" then
							table.insert(parts, ui.t("settings.usage.per_million", { price = m.per_million }):get())
						end
						return table.concat(parts, " · ")
					end,
					truncate = true,
					control = ui.row {
						gap = "$space.3",
						items = "center",
						ui.text {
							function(): string
								local m = now:get()
								return if in_cost() then m.cost_share else m.tokens_share
							end,
							tabular = true,
							text_size = "$font.size.xs",
							color = "$text.tertiary",
						},
						number(function(): string
							return shown(now:get())
						end),
					},
				}
			end
			return ds.Settings.Row {
				label = function(): string
					local f = figures:get()
					return M.period_name(now:get().start, f ~= nil and f.hourly)
				end,
				description = function(): string
					local p = now:get()
					return if in_cost() then p.tokens else p.cost
				end,
				control = number(function(): string
					return shown(now:get())
				end),
			}
		end, {
			key = function(r: any): string
				return r.key or tostring(r.start)
			end,
		}),
	}
end

function M.view(): Node
	-- Read as the page opens and whenever the range changes.
	ui.effect(function()
		range:get()
		read()
	end)
	return ds.Settings.Page {
		title = ui.t("settings.usage.title"),
		description = ui.t("settings.usage.description"),
		trail = ui.row {
			gap = "$space.2",
			items = "center",
			ds.Segmented {
				value = range,
				id = "usage-range",
				label = ui.t("settings.usage.range"),
				options = {
					{ value = "1", label = ui.t("settings.usage.range_1") },
					{ value = "7", label = ui.t("settings.usage.range_7") },
					{ value = "30", label = ui.t("settings.usage.range_30") },
					{ value = "90", label = ui.t("settings.usage.range_90") },
				} :: any,
			},
			ui.div {
				opacity = function(): number
					return if reading:get() then 0.5 else 1
				end,
				ds.IconButton { icon = "refresh", label = ui.t("settings.usage.refresh"), on_click = read },
			},
		},
		overview(),
		tokens(),
		breakdown(),
	}
end

return M
