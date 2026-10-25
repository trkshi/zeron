// Written by Zeron for one Command Code run (`--mod`): connects Zeron's own
// MCP server and offers its tools as `zeron_<tool>`. Command Code reads MCP
// servers only from its config files, so nothing global is modified instead.
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

export default async function (cmd) {
  const config = JSON.parse(process.env.ZERON_COMMANDCODE_MCP);
  const pending = new Map();
  let nextId = 0;
  const child = spawn(config.command, config.args, {
    env: { ...process.env, ...config.env },
    stdio: ["pipe", "pipe", "inherit"],
  });
  const fail = (error) => {
    for (const request of [...pending.values()]) request.reject(error);
  };
  child.on("error", fail);
  child.on("exit", () => fail(new Error("Zeron MCP server exited")));
  child.stdin.on("error", fail);
  process.once("exit", () => child.kill());
  createInterface({ input: child.stdout }).on("line", (line) => {
    let message;
    try {
      message = JSON.parse(line);
    } catch {
      return;
    }
    const request = pending.get(message.id);
    if (!request) return;
    if (message.error) request.reject(new Error(message.error.message));
    else request.resolve(message.result);
  });
  const send = (message) => child.stdin.write(JSON.stringify(message) + "\n");
  function rpc(method, params, signal) {
    if (!child.stdin.writable) return Promise.reject(new Error("Zeron MCP is not connected"));
    if (signal?.aborted) return Promise.reject(new Error("Zeron MCP request cancelled"));
    return new Promise((resolve, reject) => {
      const id = ++nextId;
      const cleanup = () => {
        clearTimeout(timer);
        signal?.removeEventListener("abort", abort);
        pending.delete(id);
      };
      const abort = () => {
        send({ jsonrpc: "2.0", method: "notifications/cancelled", params: { requestId: id } });
        cleanup();
        reject(new Error("Zeron MCP request cancelled"));
      };
      // wait_for_turn can legitimately wait several minutes.
      const timer = setTimeout(() => {
        cleanup();
        reject(new Error(`Zeron MCP ${method} timed out`));
      }, method === "tools/call" ? 660_000 : 15_000);
      pending.set(id, {
        resolve: (value) => { cleanup(); resolve(value); },
        reject: (error) => { cleanup(); reject(error); },
      });
      signal?.addEventListener("abort", abort, { once: true });
      send({ jsonrpc: "2.0", id, method, params });
    });
  }
  const text = (result) =>
    (result?.content ?? []).filter((c) => c.type === "text").map((c) => c.text).join("\n");
  let tools;
  try {
    await rpc("initialize", {
      protocolVersion: "2024-11-05",
      capabilities: {},
      clientInfo: { name: "zeron-commandcode", version: "1" },
    });
    send({ jsonrpc: "2.0", method: "notifications/initialized" });
    ({ tools } = await rpc("tools/list", {}));
  } catch (error) {
    child.kill();
    cmd.ui?.notify?.(`Zeron tools unavailable: ${error.message}`);
    return;
  }
  for (const tool of tools) {
    cmd.addTool({
      schema: {
        name: `${config.name}_${tool.name}`,
        description: tool.description ?? "",
        // Command Code's input validation reads `required` unconditionally.
        input_schema: {
          type: "object",
          properties: {},
          ...tool.inputSchema,
          required: Array.isArray(tool.inputSchema?.required) ? tool.inputSchema.required : [],
        },
      },
      readOnly: tool.annotations?.readOnlyHint === true,
      run: async ({ input, signal }) => {
        try {
          const result = await rpc("tools/call", { name: tool.name, arguments: input ?? {} }, signal);
          if (result?.isError) return { ok: false, error: text(result) || "Zeron tool failed" };
          return { ok: true, content: [{ type: "text", text: text(result) || "Done." }] };
        } catch (error) {
          return { ok: false, error: error.message };
        }
      },
    });
  }
}
