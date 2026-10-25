// omp extension: raises DynamicBar island notifications (src-tauri/src/api.rs).
// Install: copy to ~/.omp/agent/hooks/post/dynamicbar.ts

/// The slice of omp's ExtensionAPI/ExtensionContext this hook reads.
type Ctx = { cwd: string; agent?: { kind: "main" | "sub" } };
type Pi = { on(event: string, handler: (event: { toolName?: string }, ctx: Ctx) => unknown): void };

const notify = (cwd: string, body: string) =>
  fetch("http://127.0.0.1:47800/notify", {
    method: "POST",
    body: JSON.stringify({ app: "omp", title: cwd.split(/[\\/]/).filter(Boolean).pop() ?? cwd, body }),
  }).catch(() => {}); // DynamicBar not running

export default function (pi: Pi) {
  // Subagents (task tool, workpools) end constantly; only the session you're driving matters.
  const main = (ctx: Ctx) => ctx.agent?.kind !== "sub";
  pi.on("agent_end", (_e, ctx) => main(ctx) && notify(ctx.cwd, "Finished"));
  pi.on("tool_approval_requested", (_e, ctx) => notify(ctx.cwd, "Needs approval"));
  pi.on("tool_execution_start", (e, ctx) => e.toolName === "ask" && main(ctx) && notify(ctx.cwd, "Waiting for your answer"));
}
