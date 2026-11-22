// memdusa for pi (coding agent extension). Written by `memdusa install pi`;
// re-run that command instead of editing, and `memdusa uninstall pi` to remove.
//
// Every event is handed to `memdusa hook` in memdusa's canonical format, so the
// offline queue, redaction, remote mode and code state work exactly as in other
// harnesses. The extension only translates pi's events.
import { spawn } from "node:child_process"

const BIN: string = __MEMDUSA_BIN__
const AGENT = "pi"

/** Run `memdusa hook <event>` with a canonical payload; resolves to its JSON output (or null). Never throws. */
function memdusa(event: string, payload: Record<string, unknown>): Promise<any> {
  return new Promise((resolve) => {
    let out = ""
    let done = false
    const finish = (value: any) => {
      if (!done) {
        done = true
        resolve(value)
      }
    }
    try {
      const child = spawn(BIN, ["hook", event, "--agent", AGENT__MEMDUSA_EXTRA_ARGS__], { stdio: ["pipe", "pipe", "ignore"] })
      const timer = setTimeout(() => {
        child.kill()
        finish(null)
      }, 15_000)
      child.stdout.on("data", (d) => (out += d))
      child.on("error", () => finish(null))
      child.on("close", () => {
        clearTimeout(timer)
        try {
          finish(out.trim() ? JSON.parse(out) : null)
        } catch {
          finish(null)
        }
      })
      child.stdin.end(JSON.stringify(payload))
    } catch {
      finish(null)
    }
  })
}

export default function (pi: any) {
  let briefed = false
  const sessionOf = (ctx: any) => ctx.sessionManager?.getSessionId?.() ?? ""

  // New, resumed or forked session: brief again on its first run.
  pi.on("session_start", async () => {
    briefed = false
  })

  // Before a run: load the resume brief once per session, then journal the prompt.
  pi.on("before_agent_start", async (event: any, ctx: any) => {
    if (event.messageType !== "user") return
    const session_id = sessionOf(ctx)
    let message
    if (!briefed) {
      briefed = true
      const out = await memdusa("session-start", { session_id, cwd: ctx.cwd, source: "startup" })
      const brief = out?.hookSpecificOutput?.additionalContext
      if (brief) message = { customType: "memdusa", content: brief, display: false }
    }
    // Memories relevant to this prompt (none when nothing matches or recall is off).
    const recall = await memdusa("user-prompt-submit", { session_id, cwd: ctx.cwd, prompt: event.prompt ?? "" })
    const relevant = recall?.hookSpecificOutput?.additionalContext
    if (relevant) {
      message = message
        ? { ...message, content: `${message.content}\n\n${relevant}` }
        : { customType: "memdusa", content: relevant, display: false }
    }
    return message ? { message } : undefined
  })

  // File edits and shell commands.
  pi.on("tool_result", async (event: any, ctx: any) => {
    if (event.isError) return
    const input = event.input ?? {}
    let call
    if (event.toolName === "write" || event.toolName === "edit") call = { tool_name: "Edit", tool_input: { file_path: input.path } }
    else if (event.toolName === "bash") call = { tool_name: "Bash", tool_input: { command: input.command } }
    if (call) await memdusa("post-tool-use", { session_id: sessionOf(ctx), cwd: ctx.cwd, ...call })
  })

  // Last chance before pi settles: ask the agent to checkpoint if it changed files
  // (memdusa allows this at most once per session per window, so it can't loop).
  pi.on("agent_before_settle", async (event: any, ctx: any) => {
    const out = await memdusa("stop", { session_id: sessionOf(ctx), cwd: ctx.cwd, stop_hook_active: false })
    if (out?.decision !== "block" || !out.reason) return
    return {
      entries: [...(event.entries ?? []), { type: "custom_message", customType: "memdusa", content: out.reason, display: false }],
      continue: true,
    }
  })

  // Context is about to be lost: write a fallback checkpoint if the agent changed files without one.
  pi.on("session_before_compact", async (event: any, ctx: any) => {
    await memdusa("pre-compact", { session_id: sessionOf(ctx), cwd: ctx.cwd, trigger: event.reason === "manual" ? "manual" : "auto" })
  })

  pi.on("session_shutdown", async (event: any, ctx: any) => {
    await memdusa("session-end", { session_id: sessionOf(ctx), cwd: ctx.cwd, reason: event.reason ?? "other" })
  })
}
