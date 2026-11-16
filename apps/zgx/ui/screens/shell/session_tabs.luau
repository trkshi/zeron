-- The tabs on the top bar over the page (`splits` keeps them): one for each two chats
-- or more composed together, and one for a chat alone, which navigating replaces. Each
-- shows its panes' tiles stacked (three at most) with the pane focused last in front,
-- and its title. A chat dragged onto one joins it. A click goes there, a
-- drag reorders them, its × closes the tab, and a split with it (the sessions go on,
-- in the sidebar). On a new chat, a tab of its own ("New chat") after them.
--
-- The tabs stand on the page's card: the one showing is the card's own
-- surface, risen into the top bar (a folder's tab: its top corners round, its
-- feet flaring out into the card's top edge), drawn by a layer shader cut to
-- its shape; the rest lie on the bar, quiet, a hairline between neighbours,
-- a wash under the pointer.
--
--   session_tabs.Strip { window_x = fn, start = fn, right = fn }

local ds = require("@ds")
local ContextMenu = require("@ds/context_menu")
local control = require("@ds/control")
local locale = require("@ds/locale")
local SessionRow = require("@ds/session_row")
local motion = require("@ds/motion")
local TabStrip = require("@ds/tab_strip")
local route = require("@app/screens/shell/route")
local sessions = require("@account/sessions")
local splits = require("@app/screens/shell/split")

local M = {}

-- A tab: its height, its corner's slot, a pane's tile; as wide as the room
-- shares out, between its least and most, the gap between tabs.
local TAB_H, SLOT, TILE = 36, 24, 20
local TAB_MAX, TAB_MIN, TAB_GAP = 216, 144, 4
local TITLE_SIZE = ui.raw(12, "Zeron's tab title, at any text size")
-- The top bar, down to the page's card (screens/shell/page's TOP_BAR): a tab
-- stands on the card's top edge, this far under the window's top.
local BAR = 42
local DROP = BAR - TAB_H
-- How far the showing tab's feet flare out each side (the shader's `foot`),
-- and the room kept for them at the strip's ends.
local FOOT = 10
local PAD = FOOT
-- The new chat's tab's key.
local DRAFT = "draft"
local SLIDE = { duration = 150, ease = "out(3)" }
local FADE, FADE_RATE = 36, 12

-- The bar's own inks: a tab's words not showing, its wash under the pointer,
-- the hairline between tabs.
ui.theme.tokens {
	zeron = {
		strip = {
			ink_light = "alpha({text.primary}, 0.72)",
			ink_dark = "alpha({text.primary}, 0.7)",
			hover_light = "#ffffff59",
			hover_dark = "#ffffff14",
			rule_light = "alpha({text.primary}, 0.18)",
			rule_dark = "#ffffff24",
		},
	},
}
local function strip_token(name: string): string
	local kind = ui.theme.kind()
	local light = kind == "light" or kind == "high-contrast-light"
	return `$zeron.strip.{name}_{if light then "light" else "dark"}`
end

-- The showing tab's shape (crates/ui shell/tabs.rs `folder_tab_backdrop`):
-- `TAB_H` tall from `top`, its top corners round, its feet flaring out
-- `FOOT` into the card's top edge. params: [0] its x and width (feet in),
-- [1] the strip's visible span and the tabs' top, [2] the span's edge fades.
-- The layer keeps only what's inside it.
local TAB_KEEP = ui.layer_shader([[
fn tab(p: vec2<f32>, x0: f32, w: f32, top: f32, scale: f32) -> f32 {
	let foot = 10.0;
	let corner = 10.0;
	let h = 36.0;
	var x = p.x - x0;
	let y = p.y - top;
	if (w <= 0.0 || x < 0.0 || x > w || y < -1.0 || y > h + 1.0) {
		return 0.0;
	}
	if (x > w * 0.5) {
		x = w - x;
	}
	if (x < foot) {
		let d = length(vec2<f32>(x, y - (h - foot)));
		return select(0.0, clamp((d - foot) * scale + 0.5, 0.0, 1.0), y > h - foot);
	}
	let cx = foot + corner;
	if (x < cx && y < corner) {
		let d = length(vec2<f32>(cx - x, corner - y));
		return clamp((corner - d) * scale + 0.5, 0.0, 1.0);
	}
	return clamp((x - foot) * scale + 0.5, 0.0, 1.0) * clamp(y * scale + 0.5, 0.0, 1.0);
}
fn layer(input: LayerInput) -> vec4<f32> {
	let a = input.params[0];
	let b = input.params[1];
	let c = input.params[2];
	let h = tab(input.p, a.x, a.y, b.z, input.scale)
		* clamp((input.p.x - b.x) / max(c.x, 0.5), 0.0, 1.0)
		* clamp((b.y - input.p.x) / max(c.y, 0.5), 0.0, 1.0);
	return vec4<f32>(input.color.rgb, input.color.a * h);
}]])

-- The title bar's line (where its controls centre), from the window's top.
local function line(): number
	return ds.TitleBar.middle()
end
-- A tab's stacked tiles, as a fanned hand of cards: at most this many, and for each
-- place how far along it stands and how far it's turned. Alone, a tile stands square.
local STACK = 3
local FAN = { { x = 0, turn = -3 }, { x = 6, turn = 3 }, { x = 10, turn = 7 } }

-- Each tab's node, for its width (`Strip` keeps the one showing in view).
local tab_refs: { [string]: any } = {}
-- One more each time a tab's node mounts. `tab_refs` is a plain table, so what
-- looked a tab up before its node was there (the tab for a chat alone replaced
-- by one as wide) would never look again: `tab_ref` follows this.
local tabs_mounted = ui.state("shell_session_tabs_mounted", 0)
local function tab_ref(id: string): any
	tabs_mounted:get()
	return tab_refs[id]
end

local function chat_of(id: string): any
	for _, c in sessions.now:get().chats do
		if c.id == id then
			return c
		end
	end
	return nil
end

-- The ones focused, latest first.
local recent = ui.persist("shell_session_tabs_recent", {} :: { string })

-- The tabs with a chat still there, by key. (`splits` keeps them.)
local shown = ui.computed(function(): { string }
	local out: { string } = {}
	for _, key in splits.tabs:get() do
		for _, id in splits.panes(key) do
			if chat_of(id) then
				table.insert(out, key)
				break
			end
		end
	end
	return out
end)

-- A tab's panes, as many as show (`STACK`), the one focused last first: the ones
-- focused in that order, then the rest as they stand in the split.
local function stack_of(key: string): { string }
	local peers = splits.panes(key)
	local out: { string } = {}
	for _, r in recent:get() do
		if #out < STACK and table.find(peers, r) and chat_of(r) then
			table.insert(out, r)
		end
	end
	for _, peer in peers do
		if #out < STACK and not table.find(out, peer) and chat_of(peer) then
			table.insert(out, peer)
		end
	end
	return out
end

-- What a tab's corner says: of its chats, any awaiting a person first, else any
-- at work or waiting on its background work, else one finished (done, failed
-- or out of its limits) and not yet seen (the sidebar's order of what's most
-- pressing). A finished state clears as its chat is opened (`sessions.seen`).
local TAB_STATES = {
	approval = true,
	input = true,
	failed = true,
	limited = true,
	working = true,
	waiting = true,
	connecting = true,
	done = true,
}
local function tab_state(key: string): string?
	local best: string? = nil
	for _, id in splits.panes(key) do
		local c = chat_of(id)
		local kind = if c then c.status else nil
		if kind and TAB_STATES[kind] and (not best or SessionRow.URGENCY[kind] < SessionRow.URGENCY[best]) then
			best = kind
		end
	end
	return best
end

-- What a tab shows: of its chats, the one focused last.
local function face(key: string): string
	return stack_of(key)[1] or splits.panes(key)[1] or key
end

-- The new chat's tab: there while a new chat is open (not under a plugin's
-- page or settings).
local draft_shows = ui.computed(function(): boolean
	local current = route.current:get().name
	return route.chat:get().name == "home" and current ~= "plugin" and current ~= "settings"
end)
M.DRAFT = DRAFT
M.draft_shows = draft_shows

-- The tab showing: a session's (its split's), or the new chat's.
local function selected_key(): string?
	local current = route.current:get().name
	if current == "plugin" or current == "settings" then
		return nil
	end
	if route.session:get() then
		return splits.current_key()
	end
	return if draft_shows:get() then DRAFT else nil
end

-- Every tab's width: the strip's room shared out, between their least and
-- most (`Strip` keeps it).
local tab_w = ui.state("shell_session_tab_w", TAB_MAX)

-- The tab under the pointer (the hairlines beside it go).
local hover_key = ui.state("shell_session_tab_hover", nil :: string?)
local function hover(key: string, inside: boolean)
	if inside then
		hover_key:set(key)
	elseif hover_key:peek() == key then
		hover_key:set(nil)
	end
end

-- The tab before this one, in the order the strip shows them (`order`).
local function before(key: string, order: () -> { string }): string?
	local list = order()
	local at = if key == DRAFT then #list + 1 else table.find(list, key)
	return if at and at > 1 then list[at - 1] else nil
end

-- The hairline in the gap before a tab, on the bar's line: gone beside the
-- tab showing, or the one under the pointer, and first of all.
local function rule(key: string, order: () -> { string }): Node
	return ui.div {
		absolute = true,
		left = ui.raw(-TAB_GAP / 2 - 0.5, "in the gap before the tab"),
		w = 1,
		top = function(): number
			return ui.raw(line() - 8 - DROP, "a 16-point rule, on the bar's line")
		end,
		h = 16,
		pointer_events = "none",
		bg = function(): string
			return strip_token("rule")
		end,
		opacity = function(): number
			local prev = before(key, order)
			local sel, over = selected_key(), hover_key:get()
			if not prev or sel == key or sel == prev or over == key or over == prev then
				return 0
			end
			return 1
		end,
	}
end

-- A tab's own box: its height on the card's top edge, its insets, its words
-- on the bar's line, their ink (the showing tab's the page's own), and
-- under them a wash, a 30-point pill on the bar's line, fading in under the
-- pointer (not on the showing tab: it's the card).
local function tab_box(props: { [any]: any }, active: () -> boolean, washed: () -> boolean): { [any]: any }
	props.w = function(): number
		return tab_w:get()
	end
	props.h = TAB_H
	props.shrink = 0
	props.relative = true
	props.pl = ui.raw(10, "Zeron's tab's start")
	props.pr = ui.raw(4, "Zeron's tab's end")
	-- (Its middle on the bar's line: the words there, whatever the platform's.)
	props.pb = function(): number
		return ui.raw(math.max(0, 2 * (DROP + TAB_H / 2 - line())), "on the bar's line")
	end
	props.gap = ui.raw(8, "Zeron's tab's gaps")
	props.items = "center"
	props.cursor = "pointer"
	props.color = function(): string
		return if active() then "$text.primary" else strip_token("ink")
	end
	table.insert(props, 1, ui.div {
		absolute = true,
		left = 0,
		right = 0,
		top = function(): number
			return ui.raw(line() - 15 - DROP, "a 30-point pill, on the bar's line")
		end,
		h = 30,
		rounded = "$radius.md",
		pointer_events = "none",
		bg = function(): string
			return strip_token("hover")
		end,
		opacity = ui.tween(ui.computed(function(): number
			return if washed() and not active() then 1 else 0
		end), { duration = math.max(motion.ms(120), 1), ease = "out(3)" }),
	})
	return props
end

-- A tab's end: its × under the pointer and on the tab showing; else whether
-- a chat of it awaits a person or is at work.
local function corner(id: string, hovered: State<boolean>, active: () -> boolean, on_close: (() -> ())?): Node
	return ui.row {
		w = SLOT,
		h = SLOT,
		shrink = 0,
		items = "center",
		justify = "center",
		ui.presence(function(): string
			if on_close and (hovered:get() or active()) then
				return "close"
			end
			return if id == DRAFT then "" else tab_state(id) or ""
		end, function(which: string): Node
			if which == "" then
				return ui.div {}
			end
			if which ~= "close" then
				return ui.div { id = `session-tab-{id}-state`, SessionRow.state(which :: any) }
			end
			return ds.IconButton {
				label = "Close tab",
				tooltip = false,
				size = "sm",
				tone = "quiet",
				w = 20,
				h = 20,
				rounded = "$radius.sm",
				on_click = on_close,
				ds.Icon { name = "close", size = 12 },
			}
		end, { shrink = 0 }),
	}
end

-- A chat a drag carries, for the tabs: not a tab of them being moved.
local function carried(payload: any): string?
	if type(payload) ~= "table" or payload.session_tab then
		return nil
	end
	return splits.dragged(payload)
end

-- A chat focused goes first among the focused. (Its tab: `splits`.)
ui.effect(function()
	local s = route.session:get()
	if not s then
		return
	end
	local was = recent:peek()
	if was[1] ~= s.id then
		local now = { s.id }
		for _, r in was do
			if r ~= s.id and #now < 64 then
				table.insert(now, r)
			end
		end
		recent:set(now)
	end
end)

local function close(key: string)
	local before = shown:peek()
	local at = table.find(before, key)
	-- (A kept one closes as a browser's tab does, to the one beside it.)
	local alone = #splits.panes(key) == 1 and not splits.kept(key)
	local was_showing = splits.current_key() == key
	splits.close_tabs({ key })
	if was_showing then
		-- The one beside it, else a new chat. (A chat alone, not kept: always a new
		-- chat.)
		local after = shown:peek()
		local next_key = if at and not alone then after[math.min(at, #after)] else nil
		if next_key then
			route.open_session(face(next_key))
		else
			route.new_chat()
		end
	end
end

-- The tab showing closed, or the chat alone (⌘W): true if there was one.
function M.close_showing(): boolean
	local key = splits.current_key()
	if not key then
		return false
	end
	close(key)
	return true
end

-- The new chat's tab closed (⌘W on it), to the tab beside it, the last of
-- the sessions' (it stands after them): true if there was one. Alone, it
-- stays (a second ⌘W mustn't take the window).
function M.close_draft(): boolean
	if not draft_shows:peek() then
		return false
	end
	local list = shown:peek()
	local last = list[#list]
	if not last then
		return false
	end
	route.open_session(face(last))
	return true
end

-- The next or previous tab in the strip (⌃Tab, ⌃⇧Tab), as a browser's,
-- round from the end; the new chat's counts while it shows (it stands
-- last). True if it moved: with no other tab, nothing.
function M.step(by: number): boolean
	local list = table.clone(shown:peek())
	if draft_shows:peek() then
		table.insert(list, DRAFT)
	end
	if #list < 2 then
		return false
	end
	local current: string? = if route.session:peek() then splits.current_key() elseif draft_shows:peek() then DRAFT else nil
	local at = if current then table.find(list, current) else nil
	local i = if at then (at - 1 + by) % #list + 1 elseif by > 0 then 1 else #list
	local key = list[i]
	if key == current then
		return false
	end
	if key == DRAFT then
		route.new_chat()
	else
		route.open_session(face(key))
	end
	return true
end

-- Those tabs closed at once, `keep` gone to if the one showing was among them.
-- (Each of `close` in turn would open every chat on the way.)
local function close_many(list: { string }, keep: string)
	if #list == 0 then
		return
	end
	local was_showing = table.find(list, splits.current_key() or "") ~= nil
	splits.close_tabs(list)
	if was_showing then
		route.open_session(face(keep))
	end
end

-- The tabs other than this one: all of them, or those to one side.
local function others(id: string, side: string?): { string }
	local list = shown:peek()
	local me = table.find(list, id) or 0
	local out: { string } = {}
	for i, other in list do
		if other ~= id and (side == nil or (side == "left" and i < me) or (side == "right" and i > me)) then
			table.insert(out, other)
		end
	end
	return out
end

local tile_enter = { opacity = { 0, 1 }, scale = { 0.6, 1 }, duration = 220, ease = "outBack(1.6)" }
local tile_exit = { opacity = 0, scale = 0.6, duration = 120, ease = "inQuad" }

-- The tile for a pane's place in the stack: its chat's agent's mark, as
-- the sidebar's rows have it (ds/session_row `mark`). The focus moving swaps
-- each place's pane, the new one springing up there.
local function pane_tile(id: string, place: number): Node
	return ui.presence(function(): string
		return stack_of(id)[place] or ""
	end, function(chat: string): Node
		if chat == "" then
			return ui.div {}
		end
		return ui.row {
			w = TILE,
			h = TILE,
			items = "center",
			justify = "center",
			SessionRow.mark(function(): any
				return chat_of(chat) or { id = chat, title = "" }
			end, function(): boolean
				return true
			end),
		}
	end, { w = TILE, h = TILE, shrink = 0, enter = tile_enter, exit = tile_exit })
end

-- A tab's tiles: the pane focused last's in front at the far left, turned a little,
-- its shadow on the others fanned out behind it to the right. (A tab of one pane
-- has its tile alone and square: a turned node is a layer of its own.)
local function tile(id: string): Node
	local function count(): number
		return #stack_of(id)
	end
	local function stacked(): boolean
		return count() >= 2
	end
	local function place(at: number): Node
		local fan = FAN[at]
		return ui.div {
			id = `session-tab-{id}-tile-{at}`,
			absolute = true,
			top = 0,
			left = fan.x,
			w = TILE,
			h = TILE,
			rounded = "$radius.sm",
			rotate = motion.tween(ui.computed(function(): number
				return if stacked() then fan.turn else 0
			end), SLIDE),
			-- (Solid under its wash, so what's behind it doesn't show through; the
			-- one in front's shadow falls on those behind.)
			bg = function(): string
				return if stacked() then "$zeron.menu.solid" else "#00000000"
			end,
			shadow = function(): any
				return if stacked() and at == 1 then "$shadow.sm" else false
			end,
			pane_tile(id, at),
		}
	end
	local shown_places: { any } = {
		relative = true,
		shrink = 0,
		h = TILE,
		w = motion.tween(ui.computed(function(): number
			return TILE + FAN[math.clamp(count(), 1, STACK)].x
		end), SLIDE),
	}
	-- (Those behind first: each paints under the one after it.)
	for at = STACK, 2, -1 do
		table.insert(shown_places, ui.show(function(): boolean
			return count() >= at
		end, place(at)))
	end
	table.insert(shown_places, place(1))
	return ui.div(shown_places)
end

local function title(id: string): string
	local c = chat_of(face(id))
	return if c then c.title else ""
end

-- What a tab shows, still (the tab lifted by a drag, and the drag's
-- preview): its tiles as they stand, its title, and room for its end.
-- (Nothing here eases: a drag's preview has no clock for it.)
local function still_face(key: string): Node
	local count = #stack_of(key)
	local places: { any } = { relative = true, shrink = 0, h = TILE, w = TILE + FAN[math.clamp(count, 1, STACK)].x }
	for at = math.min(count, STACK), 1, -1 do
		local fan = FAN[at]
		table.insert(places, ui.div {
			absolute = true,
			top = 0,
			left = fan.x,
			w = TILE,
			h = TILE,
			rounded = "$radius.sm",
			rotate = if count >= 2 then fan.turn else 0,
			bg = if count >= 2 then "$zeron.menu.solid" else "#00000000",
			shadow = if count >= 2 and at == 1 then "$shadow.sm" else false,
			pane_tile(key, at),
		})
	end
	return ui.row {
		grow = 1,
		min_w = 0,
		h_full = true,
		pl = ui.raw(10, "Zeron's tab's start"),
		pr = ui.raw(4, "Zeron's tab's end"),
		gap = ui.raw(8, "Zeron's tab's gaps"),
		items = "center",
		ui.div(places),
		ds.FadeText { title(key), min_w = 0, grow = 1, text_size = TITLE_SIZE },
	}
end

local Tab = ui.component(function(props: { chat: string, strip: any, order: () -> { string } }): Node
	local id = props.chat
	local strip = props.strip
	local hovered = ui.use_state(false)
	-- A chat held over it, that dropping would bring in.
	local held = ui.use_state(false)
	local own = ui.ref()
	tab_refs[id] = own
	ui.on_cleanup(function()
		if tab_refs[id] == own then
			tab_refs[id] = nil
		end
	end)
	local function active(): boolean
		return selected_key() == id
	end
	return ContextMenu {
		label = ui.t("panel.tab.actions"),
		ring = "$radius.lg",
		shrink = 0,
		items = function(): { any }
			return {
				{ label = ui.t("panel.tab.close"):get(), icon = "close-thin", on_select = function()
					close(id)
				end },
				{ label = ui.t("panel.tab.close_others"):get(), on_select = function()
					close_many(others(id), id)
				end },
				{ label = ui.t("panel.tab.close_left"):get(), on_select = function()
					close_many(others(id, "left"), id)
				end },
				{ label = ui.t("panel.tab.close_right"):get(), on_select = function()
					close_many(others(id, "right"), id)
				end },
			}
		end,
		ui.row(tab_box({
			id = `session-tab-{id}`,
			ref = own,
			on_mount = function()
				tabs_mounted:set(tabs_mounted:peek() + 1)
			end,
			role = "tab",
			label = function(): string
				return title(id)
			end,
			selected = active,
			on_hover = function(inside: boolean)
				hovered:set(inside)
				hover(id, inside)
			end,
			on_click = function()
				route.open_session(face(id))
			end,
			on_aux_click = function(e: any)
				if e.button == "middle" then
					close(id)
				else
					route.aux_click(e)
				end
			end,
			focus = control.ring,
			on_key = TabStrip.keys(id, function(): { string }
				return shown:peek()
			end, function(other: string)
				-- (The keys stay on the strip, on the tab chosen.)
				route.keep_keys()
				route.open_session(face(other))
			end, function(other: string): any
				return tab_refs[other]
			end),
			-- Dragged, it lifts and follows the pointer along the strip, the others
			-- making room (`ds/tab_strip`); let go, it stays where it lands.
			-- (`session`, as a sidebar row's: carried off the strip onto a pane, it
			-- joins the split, whose panes show where it can land.)
			on_press = function(e: any)
				strip.press(e)
			end,
			on_drag = function(): any
				strip.begin(id)
				return { session_tab = id, session = face(id) }
			end,
			on_drag_end = function(payload: any, accepted: boolean)
				splits.dragging:set(false)
				strip.ended(payload, accepted)
			end,
			drag_preview = function(): Node
				return strip.preview(id)
			end,
			opacity = function(): number
				return strip.opacity(id)
			end,
			-- (A tab of these goes to the strip's own drop: a chat, a sidebar's
			-- row or a pane's head, comes here.)
			drop_when = function(payload: any): boolean
				return carried(payload) ~= nil
			end,
			on_drag_over = function(payload: any)
				local chat = carried(payload)
				if not chat then
					return
				end
				-- Dropped on it, the chat joins it.
				local can = splits.can_join(chat, id)
				if held:peek() ~= can then
					held:set(can)
				end
			end,
			on_drag_leave = function()
				held:set(false)
			end,
			on_drop = function(payload: any)
				held:set(false)
				local chat = carried(payload)
				if not chat then
					return
				end
				splits.join_tab(chat, id)
			end,
			-- A sidebar's chat resting on its middle shows it, so it can go among its
			-- panes. (Not a pane's: its source would go, ending the drag.)
			on_drag_spring = function(payload: any): boolean
				local chat = carried(payload)
				if not chat or payload.split_pane or not held:peek() or active() then
					return false
				end
				route.open_session(face(id))
				return true
			end,
			rule(id, props.order),
			tile(id),
			ds.FadeText {
				function(): string
					return title(id)
				end,
				grow = 1,
				min_w = 0,
				text_size = TITLE_SIZE,
			},
			corner(id, hovered, active, function()
				close(id)
			end),
		}, active, function(): boolean
			return hovered:get() or held:get()
		end)),
	}
end)

-- The new chat's tab, after the sessions': its pen and "New chat", showing
-- while a new chat is open. (No ×: it goes as a chat opens, or ⌘W beside
-- another tab, `close_draft`.)
local DraftTab = ui.component(function(props: { order: () -> { string } }): Node
	local hovered = ui.use_state(false)
	local function active(): boolean
		return selected_key() == DRAFT
	end
	local function word(): string
		return locale.s("ds.sidebar.new_chat")
	end
	return ui.row(tab_box({
		id = "session-tab-draft",
		role = "tab",
		label = word,
		selected = active,
		on_hover = function(inside: boolean)
			hovered:set(inside)
			hover(DRAFT, inside)
		end,
		on_click = function()
			route.new_chat()
		end,
		rule(DRAFT, props.order),
		ui.row {
			w = TILE,
			h = TILE,
			shrink = 0,
			items = "center",
			justify = "center",
			ds.Icon { name = "pen-new-square", size = 14 },
		},
		ds.FadeText { word, grow = 1, min_w = 0, text_size = TITLE_SIZE },
		corner(DRAFT, hovered, active, nil),
	}, active, function(): boolean
		return hovered:get()
	end))
end)

-- `window_x`: how far the page's room stands from the window's left, followed
-- as the sidebar moves. `start`: where the tabs start in the window.
function M.Strip(props: { window_x: () -> number, start: () -> number, right: () -> number }): Node
	-- Each edge fades in full as soon as tabs run a little past it: a band only
	-- as wide as the overflow (`fade_overflow`) is a hard edge while that's a
	-- few points. Over the last few, as the end comes to rest, it goes.
	local offset = ui.state("shell_session_tabs_offset", 0)
	local view, content = ui.ref(), ui.ref()
	local strip = TabStrip.new {
		key = "session_tab",
		items = function(): { string }
			return shown:get()
		end,
		width = function(id: string): number
			local r = tab_ref(id)
			return if r then r.width or 0 else 0
		end,
		content = content,
		offset = offset,
		selected = function(): string?
			return if route.session:get() then splits.current_key() else nil
		end,
		face = still_face,
		-- (To the place of the one standing at `index` before it moved.)
		commit = function(id: string, index: number)
			local onto = shown:peek()[index]
			if onto and onto ~= id then
				splits.move_tab(id, onto)
			end
		end,
		-- (Off the strip, the split's panes show where it can land; over
		-- it, they stay as they are.)
		on_away = function(away: boolean)
			splits.dragging:set(away)
		end,
		h = TAB_H,
		rounded = "$radius.md",
	}
	-- The tabs as the strip shows them (a carried one sliding the rest aside).
	local function order(): { string }
		return strip.order:get()
	end
	local function bands(): { number }
		local at = offset:get()
		local past = (content.width or 0) - (view.width or 0) - at
		return { math.clamp(at * FADE_RATE, 0, FADE), math.clamp(past * FADE_RATE, 0, FADE) }
	end
	-- How many tabs stand in the strip, the new chat's among them.
	local function slots(): number
		return #shown:get() + (if draft_shows:get() then 1 else 0)
	end
	-- Their width: the room shared out among them, between their least and
	-- most. (Held while a tab's carried: the strip would shrink as its gap
	-- closes.)
	ui.effect(function()
		local room = view.width or 0
		local count = math.max(slots(), 1)
		if room <= 0 or strip.busy() then
			return
		end
		local w = math.clamp((room - PAD * 2 - TAB_GAP * (count - 1)) / count, TAB_MIN, TAB_MAX)
		if math.abs(tab_w:peek() - w) > 0.5 then
			tab_w:set(w)
		end
	end)
	-- Where the tab at `at` stands in the tabs' row.
	local function slot_left(at: number): number
		return PAD + (at - 1) * (tab_w:get() + TAB_GAP)
	end
	-- Where the tab showing stands among them (the new chat's last), if it does.
	local function showing_at(): number?
		local key = selected_key()
		if key == DRAFT then
			return #order() + 1
		end
		return if key then table.find(order(), key) else nil
	end
	-- The tab showing, always clear of the overflow and its fade: as it's opened,
	-- as tabs come and go, and as the strip's room changes (the panel opening).
	-- (A person's own scrolling is left alone: this doesn't follow the offset.)
	ui.effect(function()
		local at_slot = showing_at()
		local room = view.width or 0
		local w = tab_w:get()
		if not at_slot or room <= 0 or strip.busy() then
			return
		end
		local n = slots()
		local total = PAD * 2 + n * w + math.max(n - 1, 0) * TAB_GAP
		local left = slot_left(at_slot)
		local right = left + w
		local at = offset:peek()
		local want = at
		if w + 2 * FADE >= room then
			want = left - (room - w) / 2
		elseif left - FADE < at then
			want = left - FADE
		elseif right + FADE > at + room then
			want = right + FADE - room
		end
		want = math.clamp(want, 0, math.max(0, total - room))
		if math.abs(want - at) > 0.5 then
			offset:set(want)
		end
	end)
	-- Where they start: past the sidebar's end, followed as it moves.
	local start = props.start
	-- The tab showing, as the card's own surface risen into the bar: the
	-- card's fill, kept only inside the tab's shape (`TAB_KEEP`), there as
	-- the tab is (scrolled, it goes with the strip's edge fades). In the
	-- window's own points: this stands at the window's top left.
	local surface = ui.div {
		id = "shell-tab-surface",
		absolute = true,
		left = 0,
		right = 0,
		top = 0,
		-- (A point past the bar: over the card's top edge, its 1-point hairline
		-- from the bar's foot down, so the tab runs into the card with no line
		-- between; the shape keeps to `h + 1`.)
		h = ui.raw(BAR + 1, "over the card's 1-point edge"),
		pointer_events = "none",
		bg = "$zeron.inset.bg",
		layer_shader = TAB_KEEP,
		layer_params = function(): { any }
			local here = showing_at()
			local x, w = 0, 0
			if here then
				x = start() - offset:get() + slot_left(here) - FOOT
				w = tab_w:get() + FOOT * 2
			end
			local fades = bands()
			return {
				{ x, w, 0, 0 },
				{ start(), start() + (view.width or 0), DROP, 0 },
				{ fades[1], fades[2], 0, 0 },
			}
		end,
	}
	-- From the window's left and top whatever the sidebar is doing, so they
	-- stay put as it moves; standing on the page's card's top edge. (Only the
	-- tabs' own part takes the pointer: the sidebar's "…" lies under the
	-- rest, past the margin.)
	return ui.row {
		absolute = true,
		left = function(): number
			return -props.window_x()
		end,
		right = 0,
		top = 0,
		h = BAR + 1,
		pointer_events = "none",
		surface,
		ui.row(strip.zone {
			id = "session-tabs",
			role = "tablist",
			label = "Open chats",
			relative = true,
			grow = 1,
			min_w = 0,
			h_full = true,
			ml = start,
			pt = ui.raw(DROP, "a tab stands on the card's top edge"),
			items = "start",
			window_drag = true,
			pointer_events = "auto",
			pr = function(): any
				return ui.raw(props.right(), "clear of the title bar's controls at the right")
			end,
			strip.Frame(ui.scroll_x {
				id = "session-tabs-scroll",
				ref = view,
				offset = offset,
				grow = 1,
				min_w = 0,
				h = TAB_H,
				fade_x = bands,
				ui.row {
					ref = content,
					relative = true,
					h = TAB_H,
					px = ui.raw(PAD, "room for the showing tab's feet"),
					items = "center",
					ui.row {
						-- (An empty `ui.each` still takes room.)
						w = function(): any
							return if #shown:get() == 0 then 0 else nil
						end,
						shrink = 0,
						ui.each(strip.order, function(id: string): Node
							return Tab { chat = id, strip = strip, order = order }
						end, {
							key = function(id: string): string
								return id
							end,
							direction = "row",
							gap = ui.raw(TAB_GAP, "Zeron's gap between tabs"),
							animate = motion.tab_slide(),
						}),
					},
					ui.show(function(): boolean
						return draft_shows:get()
					end, ui.div {
						ml = function(): any
							return ui.raw(if #shown:get() > 0 then TAB_GAP else 0, "Zeron's gap between tabs")
						end,
						shrink = 0,
						DraftTab { order = order },
					}),
				},
			}),
		}),
	}
end

return M
