-- Zeron v2's activity loaders, drawn as it drew them (crates/ui/src/
-- loaders.rs): small grids of round dots whose brightness travels through
-- them. The gradient is the reply's Working line in every look; the mini is
-- the classic look's (ds/look `classic_sidebar`), the working ring
-- (ds/status) everything else's.
--
--   DotLoader { kind = "mini", size = 14 }       2 × 3 dots, the brightness
--                                                 chasing round them clockwise:
--                                                 a session, subagent or tool at
--                                                 work (v2's `mini_glyph_spinner`)
--   DotLoader { kind = "gradient", size = 16 }   3 × 3 dots in the sunrise
--                                                 colours, the pulse rising from
--                                                 the bottom edge: the reply's
--                                                 Working line (`gradient_spinner`)
--
-- `size` is the square it stands in (the ring's it replaces); the dots are
-- v2's at that square (a mini dot an eighth of it, a gradient dot 1/6.4: 2
-- and 2.5 points at 16), a dot's gap half a dot, centred. `ink` a token, the
-- mini's colour (its rows lighter, as is, and deeper, as v2's accent glyph):
-- the working hue by default. Every dot rests at 0.1 and flares to full once
-- a 750 ms cycle (v2's `gspin_opacity`).
--
-- Cost: as ds/agent_loader's, all of it a shader at 30 frames a second, only
-- while it can be seen; still when motion is reduced.

local motion = require("@self/motion")

export type Kind = "mini" | "gradient"
export type Props = { kind: Kind?, size: number?, ink: string? }

local CYCLE = 0.75
local FPS = 30
local DIM = 0.1

-- Each kind's grid: its columns and rows, its dot as a share of the square,
-- and each dot's phase.
local KINDS = {
	mini = {
		cols = 2,
		rows = 3,
		dot = 1 / 8,
		-- Clockwise round the grid from the top left (v2's `RING`).
		phase = function(r: number, c: number): number
			local ring = { { 0, 1 }, { 5, 2 }, { 4, 3 } }
			return ring[r + 1][c + 1] / 6
		end,
	},
	gradient = {
		cols = 3,
		rows = 3,
		dot = 1 / 6.4,
		-- From the bottom edge in to the top centre (v2's `gspin_cell_phase`).
		phase = function(r: number, c: number): number
			local centre = 1
			local d = 2 - r + math.abs(c - centre)
			return d / (2 + centre + 1)
		end,
	},
}

-- v2's sunrise tints, a row each, top to bottom (`GSPIN_ROW_TINTS`).
local SUNRISE = { "#B6D3EF", "#EDB185", "#F888A0" }

local function num(x: number): string
	return string.format("%.6f", x)
end

local function shader(name: Kind): any
	local g = KINDS[name]
	-- In dots: the grid's width and height (gaps half a dot), each centre.
	local w, h = g.cols + (g.cols - 1) * 0.5, g.rows + (g.rows - 1) * 0.5
	-- A row's colour, as the shader reads it: the mini's from its ink, the
	-- gradient's baked in (v2's fixed sunrise tints, top to bottom). Passed
	-- as three token params instead, the top two rows came out clear and the
	-- line showed its bottom three dots alone.
	local function tint(r: number): string
		if name == "mini" then
			return `zr_row({r},input.params[0])`
		end
		local hex = SUNRISE[r + 1]
		local function ch(i: number): string
			return num(tonumber(string.sub(hex, i, i + 1), 16) / 255)
		end
		return `vec4({ch(2)},{ch(4)},{ch(6)},1.)`
	end
	local dots: { string } = {}
	for r = 0, g.rows - 1 do
		for c = 0, g.cols - 1 do
			local x = c * 1.5 + 0.5 - w / 2
			local y = r * 1.5 + 0.5 - h / 2
			table.insert(dots, ` zr_dot(q,vec2({num(x)},{num(y)}),{num(g.phase(r, c))},t,s,k,{tint(r)},&rgb,&a);`)
		end
	end
	local rows = if name == "mini"
		then [[
// A row's colour: the ink lighter, as is, and deeper (v2's accent glyph).
fn zr_row(r:i32,ink:vec4<f32>)->vec4<f32> {
 if r==0 { return vec4(mix(ink.rgb,vec3(1.),.28),ink.a); }
 if r==2 { return vec4(ink.rgb*.78,ink.a); }
 return ink;
}]]
		else ""
	return ui.shader(string.format([[
// A dot's opacity at x of its cycle: full, down to its rest by 45%%, resting,
// back to full from 92%%.
fn zr_pulse(x:f32)->f32 {
 let t=fract(x);
 if t<.45 { return mix(1.,%s,t/.45); }
 if t<.92 { return %s; }
 return mix(%s,1.,(t-.92)/.08);
}
%s
// One dot (k points across) at c (in dots from the grid's centre): its colour
// and coverage into rgb and a (the dots never overlap).
fn zr_dot(q:vec2<f32>,c:vec2<f32>,phase:f32,t:f32,s:f32,k:f32,tint:vec4<f32>,rgb:ptr<function,vec3<f32>>,a:ptr<function,f32>) {
 let d=length(q-c*k)-.5*k;
 let cov=clamp(.5-d*s,0.,1.);
 if cov>0. {
  *rgb=tint.rgb;
  *a=max(*a,cov*tint.a*zr_pulse(t+phase));
 }
}
fn fill(input:FillInput)->vec4<f32> {
 let side=min(input.size.x,input.size.y);
 let k=side*%s;
 let q=input.p-input.size*.5;
 let s=input.scale;
 let still=input.params[3].x>.5;
 let t=select(input.time/%s,0.,still);
 var rgb=vec3(0.);
 var a=0.;
%s
 return vec4(rgb,a);
}
]], num(DIM), num(DIM), num(DIM), rows, num(g.dot), num(CYCLE), table.concat(dots, "\n")))
end

local shaders: { [Kind]: any } = {}
local function shader_of(name: Kind): any
	local made = shaders[name]
	if not made then
		made = shader(name)
		shaders[name] = made
	end
	return made
end

return function(props: Props): Node
	local name: Kind = props.kind or "mini"
	local size = props.size or 14
	local period = CYCLE * 1000
	local still = if motion.reduced_now() then 1 else 0
	local params = if name == "mini"
		then { props.ink or "$zeron.status.working", { 0, 0, 0, 0 }, { 0, 0, 0, 0 }, { still, 0, 0, 0 } }
		else { { 0, 0, 0, 0 }, { 0, 0, 0, 0 }, { 0, 0, 0, 0 }, { still, 0, 0, 0 } }
	-- It plays only while it can be seen (ds/status's ring, `looping`), in
	-- step with every other as it shows, started afresh each time it does
	-- (`looping` says why).
	return ui.div {
		w = size,
		h = size,
		shrink = 0,
		aria_hidden = true,
		class = `dot-loader-{name}`,
		bg_shader = shader_of(name),
		shader_params = params,
		on_visible = function(visible: boolean, el: Element)
			anime.utils.remove(el)
			if visible and not motion.reduced_now() then
				local anim = anime.animate(el, {
					shaderTime = { 0, CYCLE },
					duration = period,
					loop = true,
					ease = "linear",
					frameRate = FPS,
				})
				anim.currentTime = ui.now() % period
			end
		end,
	}
end
