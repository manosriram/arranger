// Loaded by pi via `-e`; forwards events as JSON lines to one long-lived `arranger hook pi`.
import { spawn } from "node:child_process";

export default function (pi: any) {
  const bin = process.env.ARRANGER_BIN;
  if (!bin) return;
  const hook = spawn(bin, ["hook", "pi"], { stdio: ["pipe", "ignore", "ignore"] });
  hook.on("error", () => {});
  hook.stdin.on("error", () => {});
  hook.unref();
  (hook.stdin as any).unref?.();
  const send = (event: object) => hook.stdin.write(JSON.stringify(event) + "\n");

  pi.on("before_agent_start", async (e: any) => send({ type: "prompt", prompt: e.prompt }));
  pi.on("tool_execution_start", async (e: any) => send({ type: "tool_start", tool: e.toolName, args: e.args }));
  pi.on("tool_execution_end", async (e: any) => send({ type: "tool_end", error: !!e.isError }));
  pi.on("message_end", async (e: any) => {
    if (e.message?.role === "assistant" && e.message.usage) send({ type: "usage", usage: e.message.usage });
  });
  pi.on("agent_settled", async () => send({ type: "done" }));
}
