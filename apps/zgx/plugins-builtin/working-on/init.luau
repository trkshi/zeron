--!strict
-- Working on: under a new chat's composer (Zeron's `slot:zeron.home.below`),
-- one card of your chats on every machine, in three parts with a rule
-- between: what needs you (a question, an approval, a plan to look at, a
-- failure, a limit), what's running (at work, waiting on background work,
-- connecting), and what you worked on last (newest first; one that finished
-- and you haven't opened yet wears a dot). A part with nothing in it isn't
-- drawn, and with nothing at all, neither is the card. A row opens its chat.
--
-- Only chats you started: a subagent's or a side chat (`child_kind`) is its
-- parent's business. Each row: the agent's mark, the title, its project and
-- (when it isn't this one) its machine, and on the far side what it's doing
-- or how long ago it moved. Built from what any plugin can use: the
-- sessions' records ("sessions.read"), your devices' names ("devices"), and
-- opening a chat ("navigate").

local zeron = require("./zeron")("working-on")
local status = require("@ds/status")
local DotLoader = require("@ds/dot_loader")
local AgentMark = require("@ds/agent_mark")
local FadeText = require("@ds/fade_text")
local ListRow = require("@ds/list_row")
local locale = require("@ds/locale")

local recent_setting = zeron.setting("recent")
local loader_setting = zeron.setting("loader")

-- How many a part lists before "N more", and the card's tallest before it
-- scrolls.
local MOST_NEEDS, MOST_RUNNING = 4, 4
local MOST_H = 360
local ROW_H = 32

local RUNNING = { working = true, waiting = true, connecting = true }
local NEEDS = { input = true, approval = true, plan = true, failed = true, limited = true }
-- (Which of those comes first.)
local URGENCY = { approval = 1, input = 2, plan = 3, failed = 4, limited = 5 }
-- Finished, and not opened since: a dot on its Recent row.
local UNSEEN = { done = true, woke = true }

type Row = { id: string, status: string? }
type Groups = { needs: { Row }, running: { Row }, recent: { Row }, more_needs: number, more_running: number }

-- Now, in seconds, a minute at a time: what "5m ago" counts from.
local clock = ui.state("working_on_clock", os.time())
zeron.every(30, function()
	clock:set(os.time())
end)

-- Your chats, sorted into the three parts.
local groups = ui.computed(function(): Groups
	local needs, running, recent = {}, {}, {}
	for id, s in zeron.sessions() do
		if s.child_kind == nil then
			local st = s.status
			local row = { id = id, status = st, updated = s.updated or 0 }
			if st and NEEDS[st] then
				table.insert(needs, row)
			elseif st and RUNNING[st] then
				table.insert(running, row)
			else
				table.insert(recent, row)
			end
		end
	end
	table.sort(needs, function(a: any, b: any): boolean
		local ua, ub = URGENCY[a.status] or 9, URGENCY[b.status] or 9
		if ua ~= ub then
			return ua < ub
		end
		return a.updated > b.updated
	end)
	local newest = function(a: any, b: any): boolean
		return a.updated > b.updated
	end
	table.sort(running, newest)
	table.sort(recent, newest)
	local function cap(list: { any }, most: number): ({ Row }, number)
		local out = {}
		for i = 1, math.min(#list, most) do
			table.insert(out, { id = list[i].id, status = list[i].status })
		end
		return out, math.max(0, #list - most)
	end
	local n, more_n = cap(needs, MOST_NEEDS)
	local r, more_r = cap(running, MOST_RUNNING)
	local c = cap(recent, tonumber(recent_setting:get()) or 4)
	return { needs = n, running = r, recent = c, more_needs = more_n, more_running = more_r }
end)

-- What the card is made of, as one string: built again only when a row
-- comes, goes, moves or changes state (not as a chat's time moves).
local function signature(): string
	local g = groups:get()
	local parts = { loader_setting:get(), tostring(g.more_needs), tostring(g.more_running) }
	for _, name in { "needs", "running", "recent" } do
		table.insert(parts, "|" .. name)
		for _, row in (g :: any)[name] do
			table.insert(parts, `{row.id}:{row.status or ""}`)
		end
	end
	return table.concat(parts, ",")
end

-- A chat's record now (its title and times move without the card being
-- built again).
local function live(id: string): any
	return zeron.sessions()[id] or {}
end

-- Where it is: its project, and its machine when it isn't this one.
local function where(id: string): string
	local s = live(id)
	local bits = {}
	local project = s.project and zeron.projects()[s.project]
	if project and project.name then
		table.insert(bits, project.name)
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

local function ago(id: string): string
	return locale.ago(clock:get() - (live(id).updated or clock:get()))
end

-- A running chat's loader, as the setting asks.
local function loader(): Node
	if loader_setting:peek() == "ring" then
		return status.StatusIcon { kind = "working", size = 14 }
	end
	return DotLoader { kind = "mini", size = 14 }
end

-- One chat: its mark, title and place, and on the far side `trail`.
local function row(id: string, trail: Node): Node
	local s = live(id)
	return ListRow.ListRow {
		h = ROW_H,
		label = function(): string
			return live(id).title or zeron.s("untitled")
		end,
		on_click = function()
			zeron.open { session = id }
		end,
		ui.row {
			w_full = true,
			h_full = true,
			px = "$space.3",
			gap = "$space.2",
			items = "center",
			ui.div {
				w = 16,
				h = 16,
				shrink = 0,
				items = "center",
				justify = "center",
				AgentMark.Mark { label = s.harness or "", mark = s.agent, seed = s.harness, size = 14, bare = true },
			},
			FadeText {
				function(): string
					local t = live(id).title
					return if t and t ~= "" then t else zeron.s("untitled")
				end,
				min_w = 0,
				shrink = 1,
				text_size = "$font.size.sm",
				color = "$text.primary",
			},
			FadeText {
				function(): string
					return where(id)
				end,
				min_w = 0,
				grow = 1,
				shrink = 2,
				text_size = "$font.size.xs",
				color = "$text.tertiary",
			},
			ui.row { shrink = 0, gap = "$space.1", items = "center", trail },
		},
	}
end

local function quiet(text: string | () -> string): Node
	return ui.text { text, text_size = "$font.size.xs", color = "$text.tertiary", tabular = true, shrink = 0 }
end

local function header(word: string, count: number?): Node
	return ui.row {
		h = 26,
		px = "$space.3",
		pt = "$space.1",
		items = "center",
		gap = "$space.1",
		ui.text { word, text_size = "$font.size.xs", weight = "$font.weight.medium", color = "$text.secondary" },
		if count and count > 0 then quiet(tostring(count)) else nil,
	}
end

local function more(n: number): Node?
	if n <= 0 then
		return nil
	end
	return ui.row { h = 24, px = "$space.3", items = "center", quiet(zeron.s("more", { n = n })) }
end

local function rule(): Node
	return ui.div { h = 1, mx = "$space.3", my = "$space.1", bg = "$border.subtle", shrink = 0 }
end

-- The card for what `groups` says now.
local function Card(): Node
	local g = groups:peek()
	local parts: { { Node } } = {}
	if #g.needs > 0 then
		local nodes: { Node } = { header(zeron.s("needs_you"), #g.needs + g.more_needs) }
		for _, r in g.needs do
			local kind = r.status or "input"
			table.insert(nodes, row(r.id, ui.row {
				gap = "$space.1",
				items = "center",
				quiet(zeron.s(`state.{kind}`)),
				status.StatusIcon { kind = kind :: any, size = 14 },
			}))
		end
		local rest = more(g.more_needs)
		if rest then
			table.insert(nodes, rest)
		end
		table.insert(parts, nodes)
	end
	if #g.running > 0 then
		local nodes: { Node } = { header(zeron.s("running"), #g.running + g.more_running) }
		for _, r in g.running do
			table.insert(nodes, row(r.id, ui.row {
				gap = "$space.1",
				items = "center",
				quiet(function(): string
					return zeron.s("active", { ago = ago(r.id) })
				end),
				loader(),
			}))
		end
		local rest = more(g.more_running)
		if rest then
			table.insert(nodes, rest)
		end
		table.insert(parts, nodes)
	end
	if #g.recent > 0 then
		local nodes: { Node } = { header(zeron.s("recent")) }
		for _, r in g.recent do
			local unseen = r.status ~= nil and UNSEEN[r.status] == true
			table.insert(nodes, row(r.id, ui.row {
				gap = "$space.1",
				items = "center",
				if unseen then ui.div { label = zeron.s("unseen"), status.StatusDot { kind = "done", size = 6 } } else nil,
				quiet(function(): string
					return ago(r.id)
				end),
			}))
		end
		table.insert(parts, nodes)
	end
	if #parts == 0 then
		return ui.div {}
	end
	local children: { Node } = {}
	for i, nodes in parts do
		if i > 1 then
			table.insert(children, rule())
		end
		for _, node in nodes do
			table.insert(children, node)
		end
	end
	return ui.column {
		id = "working-on",
		w_full = true,
		role = "list",
		label = zeron.s("label"),
		rounded = "$radius.lg",
		bg = "$surface.raised",
		border = 1,
		border_color = "$border.subtle",
		overflow = "hidden",
		pointer_events = "auto",
		ui.scroll {
			w_full = true,
			max_h = MOST_H,
			py = "$space.1",
			table.unpack(children),
		},
	}
end

zeron.place {
	at = "slot:zeron.home.below",
	after = function(): Node
		return ui.presence(signature, function(_key: string): Node
			return Card()
		end, { w_full = true })
	end,
}
