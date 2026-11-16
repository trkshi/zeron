-- How full a model's context is, as the composer's ring shows it and its
-- card says it (Zeron's crates/ui/src/context_usage.rs): what the agent
-- reports (tokens in it, the window's size, either missing), how full that
-- is, the ink it reads in (the text's own, amber from three quarters, red
-- from nine tenths), and the card itself.
--
--   context_usage.fraction(u)  0 to 1, or nil while it can't be said
--   context_usage.lines(u)     the card's two lines, as Zeron writes them
--   context_usage.ink(f)       the ring's and the bar's ink at `f`
--   Card { usage = fn }        the card (ds/hover_card's), "Context window"
--                              and how full at its head, a slim bar, the
--                              lines (followed as the usage changes)
--   Detail { usage = fn }      the ring's long form, for the composer's
--                              "detail" (Settings → Appearance): "Context",
--                              a bar of segments, "223k/1.0M (22%)"

local locale = require("@self/locale")
local HoverCard = require("@self/hover_card")
local progress = require("@self/progress")

export type Usage = { tokens: number?, window: number? }

local M = {}

-- A count of tokens, its thousands grouped ("408,389").
function M.grouped(n: number): string
	local digits = string.format("%d", math.max(0, math.floor(n + 0.5)))
	local out = string.reverse((string.gsub(string.reverse(digits), "(%d%d%d)", "%1,")))
	return (string.gsub(out, "^,", ""))
end

-- A count of tokens, short: "950", "9.5k", "223k", "1.0M".
function M.short(n: number): string
	n = math.max(0, n)
	if n < 1000 then
		return string.format("%d", math.floor(n + 0.5))
	elseif n < 10000 then
		return (string.gsub(string.format("%.1f", n / 1000), "%.0$", "")) .. "k"
	elseif n < 999500 then
		return string.format("%d", math.floor(n / 1000 + 0.5)) .. "k"
	end
	return string.format("%.1f", n / 1000000) .. "M"
end

function M.fraction(u: Usage?): number?
	if u and u.tokens and u.window and u.window > 0 then
		return math.clamp(u.tokens / u.window, 0, 1)
	end
	return nil
end

-- Used of the window and what is left; or what is known of it (tokens
-- with no window, a window with nothing counted yet, nothing at all).
function M.lines(u: Usage?): (string, string?)
	local tokens = if u then u.tokens else nil
	local window = if u then u.window else nil
	if tokens and window and window > 0 then
		return locale.s("ds.composer.context.of", { used = M.grouped(tokens), window = M.grouped(window) }),
			locale.s("ds.composer.context.left", { n = M.grouped(math.max(window - tokens, 0)) })
	elseif tokens then
		return locale.s("ds.composer.context.used_only", { n = M.grouped(tokens) }), locale.s("ds.composer.context.no_limit")
	elseif window and window > 0 then
		return locale.s("ds.composer.context.capacity", { n = M.grouped(window) }), locale.s("ds.composer.context.waiting")
	end
	return locale.s("ds.composer.context.unreported"), nil
end

function M.ink(f: number): string
	return if f >= 0.9 then "$danger" elseif f >= 0.75 then "$warning" else "$text.secondary"
end

-- The ring itself: a faint track under an arc from twelve o'clock, in the
-- ink of how full it is, the chips' glyph size (12 across its ink, so it
-- reads as one of them rather than a size up).
function M.Ring(props: { fraction: () -> number, ink: ((f: number) -> string)? }): Node
	local ink = props.ink or M.ink
	return ui.canvas {
		w = 12,
		h = 12,
		shrink = 0,
		draw = function(c, w, h)
			local f = props.fraction()
			c:stroke(ui.path():arc(w / 2, h / 2, 5.25, 0, 360), "$border.strong", 1.5)
			if f > 0 then
				c:stroke(ui.path():arc(w / 2, h / 2, 5.25, -90, 360 * f), ink(f), 1.5, { cap = "round" })
			end
		end,
	}
end

-- The bar of the long form: SEGMENTS thin upright bars, as many lit (in the
-- ring's ink) as the context is full, the rest a faint track; one lit as
-- soon as anything is used.
local SEGMENTS = 20
local SEG_W = 2
local SEG_GAP = 1.5
local SEG_H = 10

function M.Bar(props: { fraction: () -> number, ink: ((f: number) -> string)? }): Node
	local ink = props.ink or M.ink
	return ui.canvas {
		w = SEGMENTS * SEG_W + (SEGMENTS - 1) * SEG_GAP,
		h = SEG_H,
		shrink = 0,
		draw = function(c, _w, h)
			local f = math.clamp(props.fraction(), 0, 1)
			local lit = if f > 0 then math.max(1, math.floor(f * SEGMENTS + 0.5)) else 0
			local on = ink(f)
			for i = 0, SEGMENTS - 1 do
				c:rect(i * (SEG_W + SEG_GAP), 0, SEG_W, h, if i < lit then on else "$border.strong", 0.5)
			end
		end,
	}
end

-- The long form: "Context", the bar, then used of the window and how full
-- ("223k/1.0M (22%)"), tabular so it doesn't jitter as it counts; the
-- figures take the ring's ink once it nears full.
function M.Detail(props: { usage: () -> Usage? }): Node
	local usage = props.usage
	local function fraction(): number
		return M.fraction(usage()) or 0
	end
	return ui.row {
		items = "center",
		gap = "$space.2",
		shrink = 0,
		ui.text {
			locale.t("ds.composer.context.label"),
			shrink = 0,
			text_size = "$font.size.sm",
			color = "$text.tertiary",
		},
		M.Bar { fraction = fraction },
		ui.text {
			function(): string
				local u = usage()
				return locale.s("ds.composer.context.brief", {
					used = M.short(if u and u.tokens then u.tokens else 0),
					window = M.short(if u and u.window then u.window else 0),
					n = math.floor(fraction() * 100 + 0.5),
				})
			end,
			tabular = true,
			shrink = 0,
			text_size = "$font.size.sm",
			color = function(): string
				local f = fraction()
				return if f >= 0.75 then M.ink(f) else "$text.tertiary"
			end,
		},
	}
end

-- A plan's usage, as Zeron's account ring reads it: the accent, so it reads
-- apart from the context ring; amber from four fifths, red from 95%.
function M.usage_ink(f: number): string
	return if f >= 0.95 then "$danger" elseif f >= 0.8 then "$warning" else "$accent"
end

local function tone(f: number): progress.Tone
	return if f >= 0.9 then "danger" elseif f >= 0.75 then "warning" else "neutral"
end

-- The card: a hover card (ds/hover_card: the menus' glass, framed once),
-- a group's inset (its heading's row, then the
-- body on the heading's text edge). The percentage sits at the heading's
-- end in the ring's ink; the figures are tabular, so they don't jitter as
-- they count.
function M.Card(props: { usage: () -> Usage? }): Node
	local usage = props.usage
	local function fraction(): number?
		return M.fraction(usage())
	end
	local function known(): boolean
		return fraction() ~= nil
	end
	return HoverCard.Card {
		id = "composer-context-card",
		w = 248,
		p = "$space.1",
		gap = 0,
		ui.row {
			h = "$zeron.menu.heading",
			px = "$space.2",
			gap = "$space.2",
			items = "center",
			ui.text {
				locale.t("ds.composer.context.heading"),
				grow = 1,
				text_size = "$font.size.xs",
				weight = "$font.weight.medium",
				color = "$text.tertiary",
			},
			ui.show(known, ui.text {
				function(): string
					return locale.s("ds.composer.context.percent", { n = math.floor((fraction() or 0) * 100 + 0.5) })
				end,
				tabular = true,
				text_size = "$font.size.xs",
				weight = "$font.weight.medium",
				color = function(): string
					return M.ink(fraction() or 0)
				end,
			}),
		},
		ui.column {
			px = "$space.2",
			pt = "$space.1",
			pb = "$space.2",
			gap = "$space.2",
			ui.show(known, progress.ProgressBar {
				value = fraction,
				size = "thin",
				w_full = true,
				label = locale.t("ds.composer.context.heading"),
				tone = function(): progress.Tone
					return tone(fraction() or 0)
				end,
			}),
			ui.column {
				gap = "$zeron.space.hair",
				ui.text {
					function(): string
						return (M.lines(usage()))
					end,
					id = "composer-context-used",
					tabular = true,
					text_size = "$font.size.sm",
					line_height = "$font.line.sm",
					weight = "$font.weight.medium",
					color = "$text.primary",
				},
				ui.show(function(): boolean
					local _, second = M.lines(usage())
					return second ~= nil
				end, ui.text {
					function(): string
						local _, second = M.lines(usage())
						return second or ""
					end,
					id = "composer-context-left",
					tabular = true,
					text_size = "$font.size.sm",
					line_height = "$font.line.sm",
					color = "$text.tertiary",
				}),
			},
		},
	}
end

return M
