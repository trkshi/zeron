--!strict
-- Working on: Zeron v2's "Working now" under a new chat's composer (Zeron's
-- `slot:zeron.home.below`), as v2 drew it (crates/ui shell/working_now.rs on
-- the old interface): a summary pill ("2 running · 1 needs input · 3 to
-- review"), and under it one panel, "Working now" and how many are at work,
-- "N done" on its right, then a row for each chat that needs you, is
-- running, or finished and hasn't been opened since, in that order, on
-- every machine. A row has two lines: its marker (a question's bubble, the
-- activity dots, a check), its title, its model and on the far side its
-- state; under them its project and branch. A row opens its chat.
--
-- Only chats you started: a subagent's or a side chat (`child_kind`) is its
-- parent's business. Built from what any plugin can use: the sessions'
-- records ("sessions.read"), your devices ("devices"), a chat's model
-- ("models.read") and opening a chat ("navigate").

local zeron = require("./zeron")("working-on")
local status = require("@ds/status")
local DotLoader = require("@ds/dot_loader")
local AgentMark = require("@ds/agent_mark")
local FadeText = require("@ds/fade_text")
local Icon = require("@ds/icon")
local locale = require("@ds/locale")

local loader_setting = zeron.setting("loader")

-- v2's measures (working_now.rs).
local HEADER_H, ROW_H = 40, 52
local MOST_W = 460
local MOST_ROWS = 4
local RADIUS = 12
local SUMMARY_H, SUMMARY_GAP = 30, 12

local WORKING = { working = true, waiting = true, connecting = true }
local ASKS = { input = true, approval = true, plan = true }
local BROKE = { failed = true, limited = true }
-- Finished, and not opened since.
local DONE = { done = true, woke = true }

-- v2's glyphs it has no copy of here (Solar Linear), as code: a plugin loads
-- no file outside its own folder.
local SPEEDOMETER = [==[<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"><path d="M22 12C22 17.5228 17.5228 22 12 22C6.47715 22 2 17.5228 2 12C2 6.47715 6.47715 2 12 2C17.5228 2 22 6.47715 22 12Z" stroke="currentColor" stroke-width="1.5"/><path d="M19 19L17.5 17.5M19 5L17.5 6.5M5 19L6.5 17.5M5 5L6.5 6.5M2 12H4M19.9998 12H21.9998M12 4.00021V2.00021" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/><path d="M15 11.9999C15 13.6567 13.6569 14.9999 12 14.9999C10.3431 14.9999 9 13.6567 9 11.9999C9 11.3696 9.43408 10.4954 9.972 9.65399C10.7764 8.39573 11.1786 7.7666 12 7.7666C12.8214 7.7666 13.2236 8.39573 14.028 9.65399C14.5659 10.4954 15 11.3696 15 11.9999Z" stroke="currentColor" stroke-width="1.5"/></svg>]==]
local HOME = [==[<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><g fill="none" stroke="currentColor" stroke-width="1.5"><path stroke-linecap="round" d="M3 10.75L10.51 4.4c.86-.727 2.12-.727 2.98 0L21 10.75"/><path d="M5 9.5V16c0 2.357 0 3.536.732 4.268C6.464 21 7.643 21 10 21h4c2.357 0 3.536 0 4.268-.732C19 19.536 19 18.357 19 16V9.5"/><path stroke-linecap="round" d="M12 21v-4"/></g></svg>]==]

local function glyph(svg: string, size: number, ink: string): Node
	return ui.svg { src = `code:{svg}`, tint = ink, w = size, h = size, shrink = 0 }
end

-- v2's panel: near-black on dark, a faint wash on light.
local function panel_bg(): string
	local kind = ui.theme.kind()
	return if kind == "light" or kind == "high-contrast-light" then "#f3f3f4" else "#141414"
end

-- Now, in seconds, a minute at a time: what "active 5m ago" counts from.
local clock = ui.state("working_on_clock", os.time())
zeron.every(30, function()
	clock:set(os.time())
end)

type Row = { id: string, kind: string }

-- Each chat that needs you, runs or is done unread, v2's order: questions
-- and failures, then running (newest first), then done (newest first).
local rows = ui.computed(function(): { Row }
	local ask, run, done = {}, {}, {}
	for id, s in zeron.sessions() do
		local st = s.status
		if s.child_kind == nil and st then
			local item = { id = id, kind = st, updated = s.updated or 0 }
			if ASKS[st] or BROKE[st] then
				table.insert(ask, item)
			elseif WORKING[st] then
				table.insert(run, item)
			elseif DONE[st] then
				table.insert(done, item)
			end
		end
	end
	local newest = function(a: any, b: any): boolean
		return a.updated > b.updated
	end
	table.sort(ask, newest)
	table.sort(run, newest)
	table.sort(done, newest)
	local out: { Row } = {}
	for _, list in { ask, run, done } do
		for _, item in list do
			table.insert(out, { id = item.id, kind = item.kind })
		end
	end
	return out
end)

local function count(match: { [string]: boolean }): number
	local n = 0
	for _, r in rows:get() do
		if match[r.kind] then
			n += 1
		end
	end
	return n
end

local function live(id: string): any
	return zeron.sessions()[id] or {}
end

-- Its project and branch, and its machine when it isn't this one.
local function where(id: string): string
	local s = live(id)
	local bits = {}
	local project = s.project and zeron.projects()[s.project]
	if project and project.name then
		table.insert(bits, project.name)
	end
	local branch = s.source_branch or s.branch
	if type(branch) == "string" and branch ~= "" then
		table.insert(bits, branch)
	end
	for _, d in zeron.devices():get() or {} do
		if d.id == s.device then
			if not d.here and d.name then
				table.insert(bits, d.name)
			end
			break
		end
	end
	return table.concat(bits, " · ")
end

-- Each chat's model as its picker names it, effort after ("deepseek-v4-flash
-- High"): asked once a chat, its harness's catalog once a device.
local EFFORT = { minimal = "Minimal", low = "Low", medium = "Medium", high = "High", xhigh = "Extra High", max = "Max", ultra = "Ultra" }
local model_names = ui.state("working_on_models", {} :: { [string]: string })
local asked: { [string]: boolean } = {}
local catalogs: { [string]: any } = {}
local function catalog(harness: string, device: string?): any
	local key = `{harness}@{device or ""}`
	if not catalogs[key] then
		catalogs[key] = zeron.models(harness, { device = device })
	end
	return catalogs[key]
end
local function model_name(id: string): string
	local known = model_names:get()[id]
	if known or asked[id] then
		return known or ""
	end
	asked[id] = true
	local function named(text: string)
		model_names:set_key({ id }, text)
	end
	local s = live(id)
	zeron.chat_model(id):after(function(m: any)
		if type(m) ~= "table" or type(m.model) ~= "string" then
			return
		end
		local effort = if type(m.effort) == "string" then EFFORT[m.effort] or m.effort else nil
		local function done(label: string)
			named(if effort then `{label} {effort}` else label)
		end
		local harness = m.provider or s.harness
		if type(harness) ~= "string" then
			done(m.model)
			return
		end
		catalog(harness, s.device):after(function(list: any)
			for _, entry in (if type(list) == "table" then list else {}) do
				if entry.id == m.model and type(entry.label) == "string" then
					done(entry.label)
					return
				end
			end
			done(m.model)
		end, function()
			done(m.model)
		end)
	end, function() end)
	return ""
end

-- Its state on the far side, and its colour.
local function state_of(r: Row): (string | () -> string, string)
	if r.kind == "input" or r.kind == "plan" then
		return zeron.s("state.input"), "$zeron.status.approval"
	elseif r.kind == "approval" then
		return zeron.s("state.approval"), "$zeron.status.approval"
	elseif r.kind == "failed" then
		return zeron.s("state.failed"), "$zeron.status.failed"
	elseif r.kind == "limited" then
		return zeron.s("state.limited"), "$zeron.status.failed"
	elseif DONE[r.kind] then
		return zeron.s("state.done"), "$zeron.status.done"
	end
	return function(): string
		return zeron.s("active", { ago = locale.ago(clock:get() - (live(r.id).updated or clock:get())) })
	end, "$text.tertiary"
end

-- Its marker: a question's bubble, the activity dots, a check, a failure's
-- mark.
local function marker(r: Row): Node
	if DONE[r.kind] then
		return Icon { name = "check", size = 12, color = "$zeron.status.done" }
	elseif ASKS[r.kind] then
		return Icon { name = "chat-round-line", size = 12, color = "$zeron.status.approval" }
	elseif BROKE[r.kind] then
		return status.StatusIcon { kind = r.kind :: any, size = 12 }
	elseif loader_setting:peek() == "ring" then
		return status.StatusIcon { kind = "working", size = 12 }
	end
	return DotLoader { kind = "mini", size = 12 }
end

local function Row(props: { row: Row }): Node
	local r = props.row
	local s = live(r.id)
	local hovered = ui.use_state(false)
	local word, ink = state_of(r)
	return ui.column {
		h = ROW_H,
		w_full = true,
		px = ui.raw(10, "v2's row inset"),
		py = ui.raw(4, "v2's row inset"),
		gap = ui.raw(4, "v2's line gap"),
		justify = "center",
		rounded = ui.raw(8, "v2's row corner"),
		cursor = "pointer",
		role = "button",
		focusable = true,
		label = function(): string
			return live(r.id).title or zeron.s("untitled")
		end,
		bg = function(): string
			return if hovered:get() then "$hover" else "#00000000"
		end,
		on_hover = function(inside: boolean)
			hovered:set(inside)
		end,
		on_click = function()
			zeron.open { session = r.id }
		end,
		ui.row {
			w_full = true,
			gap = ui.raw(8, "v2's gap"),
			items = "center",
			ui.div { w = 12, h = 12, shrink = 0, items = "center", justify = "center", marker(r) },
			FadeText {
				function(): string
					local t = live(r.id).title
					return if t and t ~= "" then t else zeron.s("untitled")
				end,
				grow = 1,
				min_w = 0,
				text_size = ui.raw(14, "v2's row title"),
				color = "$text.primary",
			},
			ui.row {
				shrink = 1,
				min_w = 0,
				max_w = ui.raw(MOST_W * 0.44, "v2's model's most"),
				gap = ui.raw(5, "v2's gap"),
				items = "center",
				AgentMark.Mark { label = s.harness or "", mark = s.agent, seed = s.harness, size = 13, bare = true },
				FadeText {
					function(): string
						return model_name(r.id)
					end,
					min_w = 0,
					text_size = ui.raw(12, "v2's small text"),
					color = "$text.tertiary",
				},
			},
			ui.text { word, shrink = 0, tabular = true, text_size = ui.raw(12, "v2's small text"), color = ink },
		},
		ui.row {
			w_full = true,
			pl = ui.raw(20, "under the title"),
			FadeText {
				function(): string
					return where(r.id)
				end,
				grow = 1,
				min_w = 0,
				text_size = ui.raw(12, "v2's small text"),
				color = "$text.tertiary",
			},
		},
	}
end

-- The summary pill: "2 running · 1 needs input · 3 to review".
local function Summary(): Node
	return ui.row {
		h = SUMMARY_H,
		w_full = true,
		justify = "center",
		ui.row {
			h_full = true,
			min_w = 0,
			px = ui.raw(14, "v2's pill inset"),
			gap = ui.raw(8, "v2's gap"),
			items = "center",
			rounded = "$radius.full",
			border = 1,
			border_color = "$border.subtle",
			bg = panel_bg,
			glyph(HOME, 13, "$text.tertiary"),
			ui.text {
				function(): string
					local parts = {}
					local running = count(WORKING)
					local asking = count(ASKS) + count(BROKE)
					local done = count(DONE)
					if running > 0 then
						table.insert(parts, zeron.s("summary.running", { n = running }))
					end
					if asking > 0 then
						table.insert(parts, zeron.s("summary.input", { n = asking }))
					end
					if done > 0 then
						table.insert(parts, zeron.s("summary.review", { n = done }))
					end
					return if #parts == 0 then zeron.s("summary.nothing") else table.concat(parts, " · ")
				end,
				text_size = ui.raw(12.5, "v2's pill text"),
				color = "$text.primary",
			},
		},
	}
end

-- The panel for the rows as they are now.
local function Panel(): Node
	local list = rows:peek()
	local items: { Node } = {}
	for _, r in list do
		table.insert(items, Row { row = r })
	end
	if #list == 0 then
		table.insert(items, ui.row {
			h = ROW_H,
			px = ui.raw(10, "v2's row inset"),
			items = "center",
			ui.text { zeron.s("empty"), text_size = ui.raw(13, "v2's empty line"), color = "$text.tertiary" },
		})
	end
	local shown = math.min(math.max(#list, 1), MOST_ROWS)
	return ui.column {
		id = "working-on",
		w_full = true,
		max_w = MOST_W,
		role = "list",
		label = zeron.s("title"),
		rounded = ui.raw(RADIUS, "v2's panel corner"),
		border = 1,
		border_color = "$border.subtle",
		bg = panel_bg,
		overflow = "hidden",
		pointer_events = "auto",
		ui.row {
			h = HEADER_H,
			px = ui.raw(14, "v2's header inset"),
			gap = ui.raw(8, "v2's gap"),
			items = "center",
			glyph(SPEEDOMETER, 14, "$text.tertiary"),
			ui.text { zeron.s("title"), text_size = ui.raw(13, "v2's header title"), color = "$text.primary" },
			ui.text {
				function(): string
					return tostring(count(WORKING) + count(ASKS) + count(BROKE))
				end,
				text_size = ui.raw(12, "v2's small text"),
				color = "$text.tertiary",
			},
			ui.div { grow = 1 },
			ui.show(function(): boolean
				return count(DONE) > 0
			end, ui.text {
				function(): string
					return zeron.s("done", { n = count(DONE) })
				end,
				text_size = ui.raw(12, "v2's small text"),
				color = "$zeron.status.done",
			}),
		},
		ui.scroll {
			w_full = true,
			h = shown * ROW_H + 8,
			px = ui.raw(4, "v2's list inset"),
			pb = ui.raw(8, "v2's list foot"),
			table.unpack(items),
		},
	}
end

-- Built again only when a row comes, goes, moves or changes state (not as a
-- chat's time moves), or the loader asked for changes.
local function signature(): string
	local parts = { loader_setting:get() }
	for _, r in rows:get() do
		table.insert(parts, `{r.id}:{r.kind}`)
	end
	return table.concat(parts, ",")
end

zeron.place {
	at = "slot:zeron.home.below",
	after = function(): Node
		return ui.column {
			w_full = true,
			items = "center",
			gap = SUMMARY_GAP,
			Summary(),
			-- (Centred: v2 stood another panel beside it; alone it sits in the
			-- middle, under the pill.)
			ui.row {
				w_full = true,
				justify = "center",
				ui.presence(signature, function(_key: string): Node
					return Panel()
				end, { w_full = true, max_w = MOST_W }),
			},
		}
	end,
}
