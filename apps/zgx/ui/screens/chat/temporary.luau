-- Temporary chats: a new chat started with the page's bookmark on. It lists
-- nowhere (the sidebar, the palette, the jumps, `@` mentions, the archived
-- lists), runs on one of your devices (not Cloud) in the project's own
-- checkout, and is deleted for good, with its subagents and side chats, once
-- its tab closes (screens/shell/split) or the app quits: the next launch
-- deletes the ones a quit left. Saved, it's a chat like any other.
--
-- A delete is kept until the engine has done it: the chat stays hidden and is
-- tried again (the connection back, the next launch), so one that failed
-- never comes back as a chat of yours.
--
--   temporary.next             the new chat's toggle (State<boolean>)
--   temporary.is(id)           temporary now (tracked)
--   temporary.peek(id)         the same, untracked
--   temporary.hidden(id)       listed nowhere: temporary, or being deleted
--                              (tracked; `peek` true: untracked)
--   temporary.mark(id)         made temporary (before it's created)
--   temporary.save(id)         kept: a chat like any other from now on
--   temporary.discard(id)      deleted for good (stopped first)
--   temporary.swept            whether the launch's sweep has run
--   temporary.gone             those deleted (or on their way) this run

local sessions = require("@account/sessions")
local route = require("@app/screens/shell/route")

local M = {}

-- The temporary chats, by id: kept across launches, so the ones a quit left
-- are found and deleted at the next.
local ids = ui.persist("chat_temporary_ids", {} :: { [string]: boolean })
M.ids = ids
-- Those being deleted, until the engine has: kept across launches too.
local deleting = ui.persist("chat_temporary_deleting", {} :: { [string]: boolean })
-- Those deleted, or on their way, this run: a route to one (Back to a closed
-- tab's) goes to a new chat in its place. (Kept through a reload.)
M.gone = ui.state("chat_temporary_gone", {} :: { [string]: boolean })
-- The ones marked as this run began: all the launch's sweep deletes (never
-- one made since, whenever the sessions come). Taken once a run: a reload
-- keeps it. (And those still being deleted from the last run are gone in
-- this one from the start.)
local cohort = ui.state("chat_temporary_cohort_set", nil :: { [string]: boolean }?)
if cohort:peek() == nil then
	local first: { [string]: boolean } = {}
	for id in ids:peek() do
		first[id] = true
	end
	cohort:set(first)
	local was = table.clone(M.gone:peek())
	for id in deleting:peek() do
		was[id] = true
	end
	M.gone:set(was)
end
-- Whether it was marked as this run began, and the launch's sweep hasn't
-- run yet: the sweep's, not the tabs' (screens/shell/split) to delete.
function M.from_last_run(id: string): boolean
	return not M.swept:peek() and (cohort:peek() or {})[id] == true
end
M.swept = ui.state("chat_temporary_swept", false)

-- Whether the new chat is started temporary. Back off once one is.
M.next = ui.state("chat_temporary_next", false)

function M.is(id: string?): boolean
	return id ~= nil and ids:get()[id] == true
end

function M.peek(id: string?): boolean
	return id ~= nil and ids:peek()[id] == true
end

-- (Gone too: deleted this run, though a list not caught up yet may still show it.)
function M.hidden(id: string?, untracked: boolean?): boolean
	if id == nil then
		return false
	end
	if untracked then
		return ids:peek()[id] == true or deleting:peek()[id] == true or M.gone:peek()[id] == true
	end
	return ids:get()[id] == true or deleting:get()[id] == true or M.gone:get()[id] == true
end

local function set_in(state: any, id: string, on: boolean): boolean
	local was = state:peek()[id] == true
	if was == on then
		return false
	end
	local now = table.clone(state:peek())
	now[id] = if on then true else nil
	state:set(now)
	return true
end

function M.mark(id: string)
	set_in(ids, id, true)
end

-- What's told of a chat saved (screens/shell/split keeps its tab).
M.on_save = nil :: ((id: string) -> ())?

function M.save(id: string)
	if set_in(ids, id, false) and M.on_save then
		M.on_save(id)
	end
end

-- (In flight, this run: one ask at a time each. A refusal keeps it owed,
-- asked again after a wait that doubles, from 5 s to 5 min, and at once as
-- the connection comes back. The engine's delete of one that isn't there is
-- done, not refused.)
local trying: { [string]: boolean } = {}
local waits: { [string]: number } = {}
local attempt: (id: string) -> ()
function attempt(id: string)
	if trying[id] then
		return
	end
	trying[id] = true
	sessions.stop(id):after(function() end, function() end)
	sessions.delete(id):after(function()
		trying[id] = nil
		waits[id] = nil
		set_in(deleting, id, false)
	end, function()
		trying[id] = nil
		local wait = math.min((waits[id] or 2500) * 2, 300000)
		waits[id] = wait
		ui.after(wait, function()
			if deleting:peek()[id] then
				attempt(id)
			end
		end)
	end)
end

-- (Once: a second ask, already on its way, does nothing.)
-- (Owed first, then unmarked: at no moment is it neither.)
function M.discard(id: string)
	set_in(deleting, id, true)
	set_in(ids, id, false)
	set_in(M.gone, id, true)
	attempt(id)
end

-- With the sessions in and the engine reachable: the launch's sweep (those a
-- quit left), then every delete still owed, again as the connection comes
-- back.
ui.effect(function()
	local ready = sessions.state:get() == "ready"
	local healthy = sessions.connection:get() == nil
	local owed = deleting:get()
	if not ready or not healthy then
		return
	end
	if not M.swept:peek() then
		M.swept:set(true)
		for id in cohort:peek() or {} do
			if ids:peek()[id] then
				M.discard(id)
			end
		end
	end
	for id in owed do
		attempt(id)
	end
end)

-- A route to one deleted (Back to a closed tab's, a launch back where one was):
-- a new chat, in its place in the history. (The route showing: one under
-- Settings is left alone until it's come back to.)
ui.effect(function()
	local c = route.current:get()
	if c.name == "session" and c.session and M.gone:get()[c.session] then
		route.leave_gone()
	end
end)

return M
