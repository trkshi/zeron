-- Your picture behind the page, as the page and the app's rail draw it
-- together: on a new chat (its hero, faded from the top), and, with the
-- chat background on (Settings → Appearance), behind an open chat too,
-- fainter, blurred, under a gradient in the window's tone.
--
-- With the cards flush (Settings → Appearance → Inset card off) and the
-- sidebar folded, the page meets the rail with no gap, and the picture runs
-- on under the rail to the window's edge (`under_rail`): the rail draws its
-- own slice of it, laid at the same place in the window, and a fade in the
-- window's tone runs from the window's edge across the rail into the page
-- (72 points in all), so the rail's icons read over it.
--
--   backdrop.kind()          "hero" (a new chat), "chat" (an open chat), or nil
--   backdrop.under_rail()    whether the picture runs on under the rail now
--   backdrop.window_w()      the window's width (the art is laid at it)
--   backdrop.Art { kind, width, beside, window }   the picture, as a ds.Wallpaper
--   backdrop.ChatScrim {}    the gradient over an open chat's picture
--   backdrop.RailFade { part = "rail" | "page" }   the fade at the rail's seam
--   backdrop.RAIL_FADE_W     how far the fade runs, from the window's edge

local ds = require("@ds")
local palettes = require("@ds/palettes")
local appearance = require("@app/screens/shell/appearance")
local route = require("@app/screens/shell/route")
local view = require("@app/screens/shell/view")
local fit = require("@app/screens/shell/fit")
local splits = require("@app/screens/shell/split")
local host = require("@app/screens/settings/host")

local M = {}

-- The fade at the rail's seam: from the window's edge, this far, in the
-- window's tone at its strongest there (upstream's rail scrim, #000000b8).
M.RAIL_FADE_W = 72
local RAIL_FADE_AT = 0.72
-- A blurred copy is made at steps of this many points, so dragging the
-- blur's slider asks for a few files, not one for every point.
local BLUR_STEP = 4

-- The window's tone over the picture: an open chat's gradient (from the
-- top, deepening down), and the rail's fade.
ui.theme.tokens {
	zeron = {
		art_scrim = {
			top = "alpha({zeron.window}, 0.1)",
			mid = "alpha({zeron.window}, 0.4)",
			foot = "alpha({zeron.window}, 0.75)",
			none = "alpha({zeron.window}, 0)",
		},
	},
}

-- The rail's width (screens/shell/view), and the window's: the page's room
-- (`fit.viewport`, the window less the rail) and the rail.
local function rail_w(): number
	return if view.app_rail then view.app_rail_w else 0
end
function M.window_w(): number
	return fit.viewport:get() + rail_w()
end

-- What the page shows of your picture now.
function M.kind(): string?
	if appearance.showing:get() ~= "image" then
		return nil
	end
	local current = route.current:get().name
	if current == "plugin" or current == "settings" then
		return nil
	end
	local name = route.chat:get().name
	if name == "home" then
		return "hero"
	end
	return if name == "session" and appearance.chat_shown() then "chat" else nil
end

-- The sidebar folded away (by you, or for room).
local function folded(): boolean
	return view.collapsed:get() or not fit.current:get().sidebar
end

-- Whether the picture runs on under the rail: the cards flush, the sidebar
-- folded, a picture showing, the page whole (not split, not under an
-- expanded panel).
M.under_rail_now = ui.computed(function(): boolean
	return view.app_rail
		and not appearance.inset_card:get()
		and folded()
		and M.kind() ~= nil
		and #splits.members:get() < 2
		and not view.expanded:get()
end)
function M.under_rail(): boolean
	return M.under_rail_now:get()
end

-- A blurred copy of a picture, as the host makes it (src/settings.rs
-- `wallpaper.blurred`, kept on disk): its path once it's there, nil while
-- it's being made. Refused or failed, the picture itself.
local baked = ui.state("shell_backdrop_baked", {} :: { [string]: string })
local asked: { [string]: boolean } = {}
local function blurred(path: string, radius: number): string?
	local key = `{path}|{radius}`
	local got = baked:get()[key]
	if got then
		return got
	end
	if not asked[key] then
		asked[key] = true
		local function bake(file: string)
			local next = table.clone(baked:peek())
			next[key] = file
			baked:set(next)
		end
		host.call("wallpaper.blurred", { path = path, radius = radius }):after(function(file: any)
			bake(if type(file) == "string" and file ~= "" then file else path)
		end, function()
			bake(path)
		end)
	end
	return nil
end

-- How blurred an open chat's picture is, snapped to `BLUR_STEP`.
local function chat_radius(): number
	return math.floor(appearance.chat_blur:get() / BLUR_STEP + 0.5) * BLUR_STEP
end

export type ArtProps = {
	-- Which picture: a new chat's ("hero") or an open chat's ("chat").
	kind: "hero" | "chat",
	-- Where it shows (besides there being a picture of this kind now).
	shown: (() -> boolean)?,
	-- How far right of its box's right edge the art's right edge stands
	-- (ds.Wallpaper's `beside`).
	beside: (() -> number)?,
	-- The window's room over and under its box (ds.Wallpaper's `window`).
	window: { top: number, bottom: number }?,
	animate: (() -> boolean)?,
}

-- The picture, laid at the window's width from its box's right edge (so the
-- page's and the rail's are one picture), through Zeron's effect (none once
-- blurred), faded from the top on a new chat and on an open chat placed
-- "hero", full height placed "full", as strong as the chat background says.
function M.Art(props: ArtProps): Node
	local kind = props.kind
	local function radius(): number
		return if kind == "chat" then chat_radius() else 0
	end
	-- (A blurred copy on its way: what stands there stays till it's here.)
	local showing = ""
	local wall = ds.Wallpaper {
		effect = function(): string
			return if radius() > 0 then "none" else appearance.effect_now()
		end,
		shown = function(): boolean
			return M.kind() == kind and (props.shown == nil or props.shown())
		end,
		width = M.window_w,
		beside = props.beside,
		window = props.window,
		animate = props.animate,
		faded = function(): boolean
			return not (kind == "chat" and appearance.chat_placement:get() == "full")
		end,
		picture = function(): any
			return appearance.image:peek()
		end,
		key = function(): string
			local image = appearance.image:get()
			if not image or appearance.showing:get() ~= "image" then
				return ""
			end
			local src: string? = image.path
			local r = radius()
			if r > 0 then
				src = blurred(image.path, r)
			end
			if src then
				showing = `{image.x}:{image.y}:{image.zoom}:{src}`
			end
			return showing
		end,
		content = function(key: string): Node
			local x, y, zoom, path = string.match(key, "^([^:]*):([^:]*):([^:]*):(.*)$")
			local image = appearance.image:peek()
			return ds.wallpaper.Picture {
				src = path or "",
				w = if image then image.w else 1,
				h = if image then image.h else 1,
				x = tonumber(x),
				y = tonumber(y),
				zoom = tonumber(zoom),
			}
		end,
	}
	if kind ~= "chat" then
		return wall
	end
	-- An open chat's: as strong as the chat background says.
	return ui.div {
		absolute = true,
		left = 0,
		right = 0,
		top = 0,
		bottom = 0,
		pointer_events = "none",
		opacity = function(): number
			return appearance.chat_opacity:get()
		end,
		wall,
	}
end

-- Over an open chat's picture (the chat background's scrim on): the
-- window's tone, faint at the top and deepening down, so the conversation
-- reads over it. Placed "hero", it clears by the foot; "full", it holds.
function M.ChatScrim(props: { shown: (() -> boolean)? }): Node
	return ui.div {
		class = "art-scrim",
		absolute = true,
		left = 0,
		right = 0,
		top = 0,
		bottom = 0,
		pointer_events = "none",
		opacity = function(): number
			local on = M.kind() == "chat" and appearance.chat_scrim:get() and (props.shown == nil or props.shown())
			return if on then 1 else 0
		end,
		gradient = function(): any
			local function at(name: string): any
				return ui.theme.get(`zeron.art_scrim.{name}`)
			end
			local full = appearance.chat_placement:get() == "full"
			return { angle = 180, stops = { { 0, at("top") }, { 0.45, at("mid") }, { 1, at(if full then "foot" else "none") } } }
		end,
	}
end

-- The fade at the rail's seam, while the picture runs on under the rail:
-- the rail's part (its whole width, from the window's edge) or the page's
-- (from its left edge, the rest of the way). One gradient in the window's
-- tone, cut in two where the rail ends.
function M.RailFade(props: { part: "rail" | "page" }): Node
	local function alpha_at(x: number): number
		return RAIL_FADE_AT * math.max(0, 1 - x / M.RAIL_FADE_W)
	end
	local function w(): number
		return if props.part == "rail" then rail_w() else math.max(0, M.RAIL_FADE_W - rail_w())
	end
	return ui.div {
		id = `shell-rail-fade-{props.part}`,
		absolute = true,
		left = 0,
		top = 0,
		bottom = 0,
		w = w,
		pointer_events = "none",
		opacity = ui.tween(ui.computed(function(): number
			return if M.under_rail() then 1 else 0
		end), { duration = 180, ease = "out(3)" }),
		gradient = function(): any
			local got = ui.theme.get("zeron.window")
			local tone: string = if type(got) == "string" then got else "#000000"
			local from = if props.part == "rail" then 0 else rail_w()
			local to = if props.part == "rail" then rail_w() else M.RAIL_FADE_W
			return {
				angle = 90,
				stops = {
					{ 0, palettes.alpha(tone, alpha_at(from)) },
					{ 1, palettes.alpha(tone, alpha_at(to)) },
				},
			}
		end,
	}
end

return M
