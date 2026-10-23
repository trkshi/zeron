// Written by Zeron. DSH's ACP bridge sends every prompt to the model, so
// slash commands only work in DSH's own UI. This plugin runs a prompt that is
// exactly a known command line (`/compact`, `/goal …`) through DSH's command
// registry instead, and holds the agent's idle signal until it settles so the
// ACP prompt ends after the command, not before.
export const name = "zeron-acp-commands";
export const inject = ["commands"];

const COMMAND = /^\/([a-z][a-z0-9_-]*)(?=$|[\t\n\r ])/u;

/** The prompt's command line, when it is a single text block starting with `/`. */
function commandLine(message) {
  const blocks = message?.content;
  if (!Array.isArray(blocks) || blocks.length !== 1) return undefined;
  const [block] = blocks;
  if (block?.type !== "text" || typeof block.text !== "string") return undefined;
  const line = block.text.trim();
  return COMMAND.test(line) ? line : undefined;
}

export function apply(ctx) {
  ctx.on("agent/created", ({ agent }) => {
    if (typeof agent?.followup !== "function" || agent.zeronCommands) return;
    agent.zeronCommands = true;
    const followup = agent.followup.bind(agent);
    const whenIdle = agent.whenIdle.bind(agent);
    let running;
    agent.followup = (message) => {
      const line = commandLine(message);
      const command = line && COMMAND.exec(line)[1];
      if (!command || ctx.commands.find(agent, command) === undefined) return followup(message);
      const run = (async () => {
        try {
          await ctx.commands.execute(agent, line, [], new AbortController().signal);
        } catch (error) {
          ctx.logger.warn(`zeron-acp-commands: /${command} failed: ${error?.message ?? error}`);
        }
        try {
          // Persist command/done now: Zeron reads it from the session log.
          if (typeof ctx.parallel === "function") await ctx.parallel("session/flush", agent.session);
        } catch {}
      })();
      running = run;
      run.finally(() => {
        if (running === run) running = undefined;
      });
    };
    agent.whenIdle = async () => {
      while (running) await running;
      return whenIdle();
    };
  });
}
