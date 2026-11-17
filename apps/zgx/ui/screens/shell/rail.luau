-- The app's rail: a column of icons down the window's left edge, left of the
-- sidebar, always there whether the sidebar is open or folded (after
-- gpui's app rail). New chat and Search, then each plugin's pages, at its
-- top; at its foot, the usage ring (your agents' plan limits, its card each
-- account's windows), then you (your profile) and Settings, which the
-- sidebar's foot row gave up for it (`view.app_rail`). (The composer keeps
-- its context ring, a chat's own, not a plan's: screens/chat/composer.)
--
--   rail.Rail()     the rail
--   rail.W          its width, which the window's fit leaves it (screens/shell/init)

local ds = require("@ds")
local AgentMark = require("@ds/agent_mark")
local Avatar = require("@ds/avatar")
local context_usage = require("@ds/context_usage")
local HoverCard = require("@ds/hover_card")
local locale = require("@ds/locale")
local PrivateText = require("@ds/private_text")
local Tooltip = require("@ds/tooltip")
local agents = require("@account/agents")
local sessions = require("@account/sessions")
local cloud = require("@account/cloud")
local boundary = require("@app/screens/shell/boundary")
local keys = require("@app/screens/shell/keys")
local route = require("@app/screens/shell/route")
local view = require("@app/screens/shell/view")
local backdrop = require("@app/screens/shell/backdrop")
local prefs = require("@app/screens/settings/prefs")

local M = {}

-- Its width, its buttons and their glyphs, the gap between them; its top
-- under the title bar, and its padding (gpui's rail's).
M.W = view.app_rail_w
local BUTTON, GLYPH, GAP, TOP, PAD = 34, 18, 6, 44, 8
local AVATAR = 24
-- The top bar over the page's card (screens/shell/page's): the picture
-- under the rail starts under it, as the card does.
local TOP_BAR = 42
-- The window's gap between the rail and the card beside it (the sidebar's,
-- or the page's with the sidebar folded): screens/shell/page's WINDOW_EDGE.
local EDGE = 5
-- Its hover eases in and out, as gpui's rail does (ds.IconButton's snaps).
local HOVER = { duration = 150, ease = "out(3)" }
-- The usage ring: its radius and stroke round the button's middle, the
-- agent's mark inside it, and its hover card's width.
local RING_RADIUS, RING_STROKE, RING_GLYPH = 13, 2.5, 14
local CARD_W = 290

-- A label with its shortcut, for a tooltip ("New chat (⌘N)").
local function with_key(label: string, id: string): string
	return locale.s("shell.rail.with_key", { label = label, key = ui.shortcut_text(keys.combo(id)) })
end

-- The shell's sidebar module (you, your profile, settings' toggle), as it's
-- loaded now; nil while it fails.
local function sidebar(): any
	return boundary.load("@app/screens/shell/sidebar")
end

type ButtonProps = {
	key: string,
	label: () -> string,
	tooltip: (() -> string)?,
	-- Its tooltip held back (a menu of its own open beside it).
	quiet: (() -> boolean)?,
	lit: (() -> boolean)?,
	on_click: (() -> ())?,
	-- A menu's anchor: whether its menu is open.
	expanded: (() -> boolean)?,
	[number]: Node,
}

-- A rail button: its glyph in the secondary ink, brightening with a wash
-- under the pointer and while it's lit (its page is open), both easing in
-- and out together.
local RailButton = ui.component(function(props: ButtonProps): Node
	local over = ui.use_state(false)
	local bright = ui.computed(function(): number
		return if over:get() or (props.lit ~= nil and props.lit()) then 1 else 0
	end)
	local tip: Node? = nil
	if props.tooltip then
		tip = if props.quiet
			then ui.show(function(): boolean
				return not (props.quiet :: () -> boolean)()
			end, Tooltip { props.tooltip })
			else Tooltip { props.tooltip }
	end
	return ui.button {
		id = props.key,
		label = props.label,
		tooltip = tip,
		tooltip_placement = { side = "right", align = "center", offset = 6 },
		expanded = props.expanded,
		relative = true,
		w = BUTTON,
		h = BUTTON,
		shrink = 0,
		p = 0,
		items = "center",
		justify = "center",
		rounded = "$radius.lg",
		bg = "#00000000",
		hover = { bg = "#00000000" },
		active = { bg = "$pressed" },
		cursor = if props.on_click then "pointer" else nil,
		color = ui.tween(bright, { from = "$text.secondary", to = "$text.primary", duration = HOVER.duration, ease = HOVER.ease }),
		on_hover = function(inside: boolean)
			over:set(inside)
		end,
		on_click = props.on_click,
		-- Its wash, fading in and out.
		ui.div {
			absolute = true,
			left = 0,
			right = 0,
			top = 0,
			bottom = 0,
			rounded = "$radius.lg",
			bg = "$hover",
			opacity = ui.tween(bright, HOVER),
			pointer_events = "none",
		},
		props[1],
	}
end)

local function new_chat(): Node
	return RailButton {
		key = "rail-new-chat",
		label = function(): string
			return locale.s("ds.sidebar.new_chat")
		end,
		tooltip = function(): string
			return with_key(locale.s("ds.sidebar.new_chat"), "shell.new_chat")
		end,
		lit = function(): boolean
			return route.current:get().name == "home"
		end,
		on_click = function()
			route.new_chat()
		end,
		ds.Icon { name = "pen-new-square", size = GLYPH },
	}
end

local function search(): Node
	return RailButton {
		key = "rail-search",
		label = function(): string
			return locale.s("ds.sidebar.search")
		end,
		tooltip = function(): string
			return locale.s("shell.rail.with_key", { label = locale.s("ds.sidebar.search"), key = ui.shortcut_text("cmd+k") })
		end,
		lit = function(): boolean
			return view.palette:get()
		end,
		on_click = function()
			view.palette:set(true)
		end,
		ds.Icon { name = "magnifer", size = GLYPH },
	}
end

-- Each plugin's pages, as the sidebar lists them (each follows its page: a
-- plugin renaming one or changing its icon shows here in place).
local function pages(): Node
	local list = ui.shared("plugins.pages", {} :: { any })
	return ui.each(list, function(_: any, _i: number, now: State<any>): Node
		local function title(): string
			return now:get().title or ""
		end
		return RailButton {
			key = `rail-page-{now:peek().plugin}-{now:peek().id}`,
			label = title,
			tooltip = title,
			lit = function(): boolean
				local c = route.current:get()
				local p = now:get()
				return c.name == "plugin" and (c :: any).plugin == p.plugin and (c :: any).page == p.id
			end,
			on_click = function()
				local p = now:peek()
				route.plugin_page(p.plugin, p.id)
			end,
			ui.presence(function(): string
				return now:get().icon or "widget"
			end, function(icon: string): Node
				return ds.Icon { name = icon :: any, size = GLYPH }
			end),
		}
	end, {
		key = function(p: any): string
			return `{p.plugin}/{p.id}`
		end,
		items = "center",
		gap = ui.raw(GAP, "the rail's gap, as gpui's"),
	})
end

-- You: your avatar, opening your profile (lit while it's open); nothing
-- until the engine has said who you are.
local function account(): Node
	local function who(): any
		local s = sidebar()
		return if s then s.user() else { name = "" }
	end
	local function name(): string
		return who().name or ""
	end
	return ui.show(function(): boolean
		return name() ~= ""
	end, RailButton {
		key = "rail-account",
		label = name,
		tooltip = name,
		lit = function(): boolean
			local s = sidebar()
			return s ~= nil and s.account.user_open()
		end,
		on_click = function()
			local s = sidebar()
			if s then
				s.account.on_user()
			end
		end,
		ui.presence(function(): string
			local u = who()
			return u.id or u.name or ""
		end, function(seed: string): Node
			return Avatar {
				seed = seed,
				size = AVATAR,
				label = name(),
				src = function(): string?
					return who().avatar
				end,
			}
		end),
	})
end

-- Settings: a toggle, lit while a page of theirs other than your profile is
-- open, a click again leaving them (the sidebar's foot button's own).
local function settings(): Node
	local function open(): boolean
		local s = sidebar()
		return s ~= nil and s.account.settings_open()
	end
	local function name(): string
		return locale.s(if open() then "shell.rail.close_settings" else "ds.sidebar.settings")
	end
	return RailButton {
		key = "rail-settings",
		label = name,
		tooltip = function(): string
			return with_key(name(), "shell.settings")
		end,
		lit = open,
		on_click = function()
			local s = sidebar()
			if s then
				s.account.on_settings()
			end
		end,
		ds.Icon { name = "settings", size = GLYPH },
	}
end

-- Usage: each agent's account in use on the machine you're working on (the
-- open chat's, or a new chat's as its composer is set), with how much of its plan's
-- windows (a session's, a week's) it has used, as the agents report them
-- (account/agents). Clicked, Settings → Usage.

-- The accounts' key the ring reads (account/agents `on_device`): "" for
-- this device, "cloud" for Cloud's, else the open chat's device, or on a
-- new chat the one its composer is set to (screens/chat/composer's
-- `chat_device`), as screens/chat/shared `account_key` has it: an account
-- switched on another machine shows while you're working there.
local chat_device = ui.state("chat_device", nil :: string?)
-- The accounts' key for a chat's `device` (see `ring_key`).
local function key_of(device: string?): string
	if device == nil or device == "" then
		return ""
	end
	if device == "cloud" then
		return "cloud"
	end
	for _, d in sessions.devices:get() or {} do
		if d.here and d.id == device then
			return ""
		end
	end
	if cloud.on_device(device) or device == cloud.device() then
		return "cloud"
	end
	return device
end

local function ring_key(): string
	local id = route.current:get().session
	local device: string? = nil
	if id then
		for _, c in sessions.now:get().chats do
			if c.id == id then
				device = c.device
				break
			end
		end
	else
		device = chat_device:get()
	end
	return key_of(device)
end

-- While a Claude Code chat works on the ring's machine, its usage as the
-- engine has it, every few seconds (asking no provider: each request the
-- chat makes reports the plan's windows in its rate-limit headers, and the
-- engine keeps them), and once more as the last of them stops.
local PEEK_MS = 10000
local function claude_at_work(key: string): number
	local n = 0
	for _, c in sessions.now:get().chats do
		if c.harness == "claude-code" and c.status == "working" and key_of(c.device) == key then
			n += 1
		end
	end
	return n
end
ui.every(PEEK_MS, function()
	local key = ring_key()
	if claude_at_work(key) > 0 then
		agents.peek_device(key)
	end
end)
local was_at_work = 0
ui.effect(function()
	local key = ring_key()
	local now = claude_at_work(key)
	if now < was_at_work then
		agents.peek_device(key)
	end
	was_at_work = now
end)

-- The agents Zeron drives, in the catalog's order: their names and marks.
local catalog = ui.shared("chat.catalog", { state = "looking", harnesses = {}, models = {} } :: any)

type Live = { harness: any, account: any }

-- One an agent: its account in use, while that reports windows.
local function live_accounts(): { Live }
	local on = agents.on_device:get()[ring_key()]
	local out: { Live } = {}
	if not on then
		return out
	end
	for _, h in catalog:get().harnesses or {} do
		for _, account in on[h.id] or {} do
			if account.active and account.windows and #account.windows > 0 then
				table.insert(out, { harness = h, account = account })
				break
			end
		end
	end
	return out
end

-- How much of its tightest window an account has used (0 to 1).
local function most_used(account: any): number
	local most = 0
	for _, w in account.windows do
		most = math.max(most, math.clamp(w.used, 0, 1))
	end
	return most
end

-- The account nearest its limit, which the ring shows.
local function tightest(list: { Live }): Live?
	local best: Live? = nil
	for _, live in list do
		if best == nil or most_used(live.account) >= most_used(best.account) then
			best = live
		end
	end
	return best
end

-- A window's name, as said here ("5h", "Weekly"), else as the agent says it.
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

-- The card's clock for its countdowns, a minute at a time (they say minutes
-- at most). One timer for the module, sleeping between ticks.
local clock = ui.state("shell_rail_usage_clock", os.time())
ui.every(30000, function()
	clock:set(os.time())
end)

-- How long until `at` (seconds since the epoch): "3d 4h", "2h 14m", "9m",
-- "under a minute".
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

local function mark_of(h: any, size: number): Node
	return AgentMark.Mark { label = h.label, mark = h.mark, icon = h.icon, seed = h.id, size = size, bare = true }
end

-- The ring's name: the account nearest its limit and its windows.
local function ring_label(): string
	local shown = tightest(live_accounts())
	if not shown then
		local loading = agents.on_device:get()[ring_key()] == nil
		return locale.s(if loading then "shell.rail.usage.loading" else "shell.rail.usage.none")
	end
	if most_used(shown.account) >= 1 then
		return locale.s("shell.rail.usage.limit")
	end
	local parts: { string } = {}
	for _, w in shown.account.windows do
		table.insert(parts, `{window_name(w.label)} {percent(w.used)}`)
	end
	return locale.s("shell.rail.usage.line", { name = shown.harness.label, windows = table.concat(parts, ", ") })
end

-- The card's accounts as they stand, by key (`{agent}:{account}`), and the
-- window that binds (the tightest account's fullest): read by its rows, so a
-- refresh moves their bars and figures in place rather than drawing them
-- again.
type Board = { order: { string }, by_key: { [string]: Live }, binding: { key: string, label: string }? }

local function live_key(live: Live): string
	return `{live.harness.id}:{live.account.id}`
end

local function window_of(live: Live?, label: string): any
	if not live then
		return nil
	end
	for _, w in live.account.windows do
		if w.label == label then
			return w
		end
	end
	return nil
end

-- A window of an account: its name, a bar of how much is used (amber from
-- 80%, red from 95%), and the share; the binding one at its limit says so.
-- Each part follows the window as the card's data changes.
local function window_row(board: State<Board>, key: string, label: string): Node
	local function used(): number
		local w = window_of(board:get().by_key[key], label)
		return if w then math.clamp(w.used, 0, 1) else 0
	end
	local function hot(): boolean
		local w = window_of(board:get().by_key[key], label)
		return w ~= nil and w.used >= 1
	end
	local function binding(): boolean
		local b = board:get().binding
		return b ~= nil and b.key == key and b.label == label
	end
	local function resets(): number?
		local w = window_of(board:get().by_key[key], label)
		return if w then w.resets else nil
	end
	return ui.column {
		gap = ui.raw(2, "a window's bar and the line under it, as gpui's card"),
		ui.row {
			items = "center",
			gap = "$space.2",
			ui.text {
				window_name(label),
				w = ui.raw(62, "gpui's window column"),
				shrink = 0,
				truncate = true,
				color = function(): string
					return if hot() then "$danger" else "$text.secondary"
				end,
			},
			ds.ProgressBar {
				value = used,
				tone = function(): any
					local u = used()
					return if u >= 0.95 then "danger" elseif u >= 0.8 then "warning" else "accent"
				end,
				grow = 1,
			},
			ui.text {
				function(): string
					return percent(used())
				end,
				w = ui.raw(44, "gpui's percent column"),
				shrink = 0,
				tabular = true,
				align = "right",
				color = function(): string
					return if hot() then "$danger" else "$text.primary"
				end,
			},
		},
		-- Under the bar: when it starts over, counting down, and at its
		-- limit (the binding one) that it's reached.
		ui.show(function(): boolean
			return resets() ~= nil or (hot() and binding())
		end, ui.text {
			function(): string
				local at = resets()
				if not at then
					return locale.s("shell.rail.usage.limit_reached")
				end
				local word = if hot() and binding() then "limit_resets_in" else "resets_in"
				return locale.s(`shell.rail.usage.{word}`, { when = until_words(at) })
			end,
			pl = ui.raw(70, "under the bar: the name's column and its gap"),
			tabular = true,
			color = function(): string
				return if hot() then "$danger" else "$text.tertiary"
			end,
		}),
	}
end

-- An agent's account: its mark, name and plan, its email (hidden while
-- addresses are), what went wrong asking, then each window, all read from
-- the card's data as it changes.
local function account_section(board: State<Board>, key: string): Node
	local function live(): Live?
		return board:get().by_key[key]
	end
	local function account(): any
		local l = live()
		return if l then l.account else {}
	end
	-- (The agent of a key never changes: its mark and name are fixed.)
	local first = board:peek().by_key[key]
	local harness = if first then first.harness else { id = "", label = "" }
	return ui.column {
		gap = "$space.2",
		ui.row {
			items = "center",
			gap = "$zeron.space.tight",
			mark_of(harness, 13),
			ui.text { harness.label, shrink = 0, weight = "$font.weight.medium", text_size = "$font.size.sm" },
			ui.show(function(): boolean
				local plan = account().plan
				return plan ~= nil and plan ~= ""
			end, ui.text {
				function(): string
					return account().plan or ""
				end,
				shrink = 0,
				color = "$text.secondary",
			}),
			ui.div { grow = 1 },
			PrivateText {
				function(): string
					return account().email or ""
				end,
				reveal = false,
				truncate = true,
				shrink = 1,
				min_w = 0,
				color = "$text.secondary",
				text_size = "$font.size.xs",
			},
		},
		ui.show(function(): boolean
			return account().error ~= nil
		end, ui.text {
			function(): string
				return account().error or ""
			end,
			wrap = true,
			color = "$warning",
		}),
		ui.each(ui.computed(function(): { string }
			local labels: { string } = {}
			for _, w in account().windows or {} do
				table.insert(labels, w.label)
			end
			return labels
		end), function(label: string): Node
			return window_row(board, key, label)
		end, {
			key = function(label: string): string
				return label
			end,
			w_full = true,
			gap = "$space.2",
		}),
	}
end

-- The ring's card: each account's windows, else what the ring says.
local function hover_card(): Node
	local board = ui.computed(function(): Board
		local list = live_accounts()
		local best = tightest(list)
		local binding: { key: string, label: string }? = nil
		if best then
			local fullest: any = nil
			for _, w in best.account.windows do
				if fullest == nil or w.used > fullest.used then
					fullest = w
				end
			end
			if fullest then
				binding = { key = live_key(best), label = fullest.label }
			end
		end
		local order: { string } = {}
		local by_key: { [string]: Live } = {}
		for _, live in list do
			local key = live_key(live)
			table.insert(order, key)
			by_key[key] = live
		end
		return { order = order, by_key = by_key, binding = binding }
	end)
	local order = ui.computed(function(): { string }
		return board:get().order
	end)
	return HoverCard.Card {
		id = "rail-usage-hover-card",
		w = function(): any
			return if #order:get() > 0 then CARD_W else "auto"
		end,
		p = 0,
		gap = 0,
		text_size = "$font.size.xs",
		ui.switch(function(): string
			return if #order:get() > 0 then "accounts" else "none"
		end, {
			accounts = ui.column {
				w_full = true,
				gap = 0,
				ui.column {
					w_full = true,
					p = "$space.2",
					gap = "$space.3",
					ui.row {
						w_full = true,
						items = "baseline",
						justify = "between",
						ui.text { ui.t("shell.rail.usage.title"), weight = "$font.weight.medium", text_size = "$font.size.sm" },
						ui.text { ui.t("shell.rail.usage.used"), color = "$text.secondary" },
					},
					ui.column {
						w_full = true,
						gap = "$space.3",
						ui.each(order, function(key: string): Node
							return account_section(board, key)
						end, {
							key = function(key: string): string
								return key
							end,
							gap = "$space.3",
						}),
					},
				},
				ui.div { w_full = true, h = 1, bg = "$border.subtle" },
				ui.row {
					w_full = true,
					px = "$space.2",
					py = "$space.2",
					items = "center",
					justify = "between",
					color = "$text.secondary",
					ui.text { ui.t("shell.rail.usage.open_page") },
					ds.Icon { name = "alt-arrow-right", size = 12 },
				},
			},
			none = ui.text { ring_label, px = "$space.2", py = "$space.1", color = "$text.primary", wrap = true },
		}),
	}
end

-- The ring: the account nearest its limit, how much of it is used round
-- its agent's mark (red, with a badge, at its limit). Hovered, its card
-- (asking the agents again); pressed, Settings → Providers, its accounts.
local function usage_ring(): Node
	ui.effect(function()
		agents.watch_device(ring_key())
	end)
	local over = ui.use_state(false)
	local clicked = ui.use_state(false)
	local function shown(): Live?
		return tightest(live_accounts())
	end
	local function hot(): boolean
		local s = shown()
		return s ~= nil and most_used(s.account) >= 1
	end
	local function lit(): boolean
		local c = route.current:get()
		return c.name == "settings" and (c :: any).page == "providers"
	end
	local bright = ui.computed(function(): number
		return if over:get() or lit() then 1 else 0
	end)
	return ui.button {
		id = "rail-usage",
		label = ring_label,
		tooltip = ui.show(function(): boolean
			return not clicked:get()
		end, hover_card()),
		tooltip_placement = { side = "right", align = "end", offset = 6 },
		tooltip_delay = 120,
		relative = true,
		w = BUTTON,
		h = BUTTON,
		shrink = 0,
		p = 0,
		items = "center",
		justify = "center",
		rounded = "$radius.lg",
		bg = "#00000000",
		hover = { bg = "#00000000" },
		active = { bg = "$pressed" },
		cursor = "pointer",
		color = "$text.primary",
		on_hover = function(inside: boolean)
			over:set(inside)
			if inside then
				agents.refresh_device(ring_key())
			else
				clicked:set(false)
			end
		end,
		on_click = function()
			clicked:set(true)
			route.settings("usage")
		end,
		ui.div {
			absolute = true,
			left = 0,
			right = 0,
			top = 0,
			bottom = 0,
			rounded = "$radius.lg",
			bg = "$hover",
			opacity = ui.tween(bright, HOVER),
			pointer_events = "none",
		},
		ui.canvas {
			absolute = true,
			left = 0,
			top = 0,
			w = BUTTON,
			h = BUTTON,
			pointer_events = "none",
			draw = function(c, w, h)
				local s = shown()
				local f = if hot() then 1 elseif s then most_used(s.account) else 0
				c:stroke(ui.path():arc(w / 2, h / 2, RING_RADIUS, 0, 360), "$border", RING_STROKE)
				if f > 0 then
					local ink = if hot() then "$danger" else context_usage.usage_ink(f)
					c:stroke(ui.path():arc(w / 2, h / 2, RING_RADIUS, -90, 360 * f), ink, RING_STROKE, { cap = "round" })
				end
			end,
		},
		ui.presence(function(): string
			local s = shown()
			return if s then s.harness.id else ""
		end, function(id: string): Node
			for _, h in catalog:peek().harnesses or {} do
				if h.id == id then
					return mark_of(h, RING_GLYPH)
				end
			end
			return ui.div {}
		end),
		ui.show(hot, ui.row {
			id = "rail-usage-limit",
			absolute = true,
			top = ui.raw(1, "gpui's badge on the ring's corner"),
			right = 0,
			w = 12,
			h = 12,
			items = "center",
			justify = "center",
			rounded = "$radius.full",
			bg = "$danger",
			pointer_events = "none",
			ui.text { "!", text_size = "$font.size.xs", weight = "$font.weight.bold", color = "$accent.text" },
		}),
	}
end

-- Your picture under the rail, while it runs on from the page (the cards
-- flush, the sidebar folded: screens/shell/backdrop): the rail's slice of
-- it, laid where the page's is in the window, under the top bar as the
-- page's card is; over it, the fade from the window's edge, so the icons
-- read.
local function under_art(): Node
	local function beside(): number
		-- (From the rail's right edge to the window's: the page's room.)
		return backdrop.window_w() - M.W
	end
	local function animate(): boolean
		return prefs.animate_new_chat:get()
	end
	local window = { top = TOP_BAR, bottom = 0 }
	return ui.div {
		id = "rail-art",
		absolute = true,
		left = 0,
		top = ui.raw(TOP_BAR, "the top bar, as the page's card"),
		bottom = 0,
		w = M.W,
		overflow = "hidden",
		pointer_events = "none",
		-- (Its own fade too, as the rail's fade has (backdrop.RailFade): a
		-- picture's own fade-out (ds/wallpaper's) has been seen stuck shown
		-- after the picture left the rail, until a relaunch; this one has not.)
		opacity = ui.tween(ui.computed(function(): number
			return if backdrop.under_rail() then 1 else 0
		end), { duration = 180, ease = "out(3)" }),
		backdrop.Art { kind = "hero", shown = backdrop.under_rail, beside = beside, window = window, animate = animate },
		backdrop.Art { kind = "chat", shown = backdrop.under_rail, beside = beside, window = window, animate = animate },
		backdrop.ChatScrim { shown = backdrop.under_rail },
		backdrop.RailFade { part = "rail" },
	}
end

M.Rail = ui.component(function(): Node
	return ui.column {
		id = "app-rail",
		role = "toolbar",
		label = function(): string
			return locale.s("shell.rail.label")
		end,
		w = M.W,
		h_full = true,
		shrink = 0,
		relative = true,
		-- (Over your picture, its icons need every bit of contrast they can
		-- get: the quieter ink brightened.)
		theme = function(): any
			return if backdrop.under_rail() then { ["text.secondary"] = "$text.primary" } else nil
		end,
		under_art(),
		pt = ui.raw(TOP + PAD, "under the title bar, as gpui's rail starts"),
		pb = ui.raw(PAD, "gpui's rail padding"),
		-- (The window's gap that follows it, before the sidebar's card or the
		-- page's, read as the rail's own: so its buttons centre in the strip
		-- the eye sees from the window's edge to the card, not 2.5 short.)
		pl = ui.raw(EDGE, "the window's gap after the rail, matched before it"),
		gap = ui.raw(GAP, "gpui's rail gap"),
		items = "center",
		-- (Its empty stretches move the window, as the title bar does.)
		window_drag = true,
		new_chat(),
		search(),
		pages(),
		ui.div { grow = 1 },
		usage_ring(),
		account(),
		settings(),
	}
end)

return M
