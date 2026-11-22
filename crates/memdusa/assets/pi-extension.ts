// memdusa for pi (coding agent extension). Written by `memdusa install pi`;
// re-run that command instead of editing, and `memdusa uninstall pi` to remove.
//
// Every event is handed to `memdusa hook` in memdusa's canonical format, so the
// offline queue, redaction, remote mode and code state work exactly as in other
// harnesses. The extension only translates pi's events.
import { spawn } from "node:child_process"
import { appendFileSync, mkdirSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"

const BIN: string = __MEMDUSA_BIN__
const AGENT = "pi"

/** One line in `$MEMDUSA_HOME/logs/plugin.log` (default `~/.memdusa/logs`), so failures are visible. Never throws. */
function logError(what: string, err?: unknown) {
  try {
    const dir = join(process.env.MEMDUSA_HOME || join(homedir(), ".memdusa"), "logs")
    mkdirSync(dir, { recursive: true })
    const detail = err instanceof Error ? err.message : err === undefined ? "" : String(err)
    appendFileSync(join(dir, "plugin.log"), `${new Date().toISOString()} ${AGENT}: ${what}${detail ? `: ${detail}` : ""}\n`)
  } catch {}
}

/** The text of the newest assistant message among pi's `agent_end` messages. */
function lastAssistantText(messages: any): string {
  if (!Array.isArray(messages)) return ""
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]
    if (m?.role !== "assistant") continue
    const content = m.content
    if (typeof content === "string") return content.trim()
    return (Array.isArray(content) ? content : [])
      .filter((p: any) => p?.type === "text" && typeof p.text === "string")
      .map((p: any) => p.text)
      .join("\n")
      .trim()
  }
  return ""
}

/**
 * The environment for `memdusa hook`. The harness may run plugins in a long-lived shared process whose environment
 * comes from whoever started it first (OpenCode's background service does), so a stale MEMDUSA_PROJECT there would
 * file every session under the wrong project. The payload's `cwd` is the reliable source: drop the variable.
 */
function hookEnv(): NodeJS.ProcessEnv {
  const env = { ...process.env }
  delete env.MEMDUSA_PROJECT
  return env
}

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
      const child = spawn(BIN, ["hook", event, "--agent", AGENT__MEMDUSA_EXTRA_ARGS__], { stdio: ["pipe", "pipe", "ignore"], env: hookEnv() })
      const timer = setTimeout(() => {
        logError(`hook ${event} timed out`)
        child.kill()
        finish(null)
      }, 15_000)
      child.stdout.on("data", (d) => (out += d))
      child.on("error", (e) => {
        logError(`hook ${event} could not run ${BIN}`, e)
        finish(null)
      })
      child.on("close", (code) => {
        clearTimeout(timer)
        if (code) logError(`hook ${event} exited with code ${code}`)
        try {
          finish(out.trim() ? JSON.parse(out) : null)
        } catch (e) {
          logError(`hook ${event} printed invalid JSON`, e)
          finish(null)
        }
      })
      child.stdin.end(JSON.stringify(payload))
    } catch (e) {
      logError(`hook ${event} failed to start`, e)
      finish(null)
    }
  })
}

export default function (pi: any) {
  let briefed = false
  let lastMessage = ""
  const sessionOf = (ctx: any) => ctx.sessionManager?.getSessionId?.() ?? ""
  // Register a handler that logs a failure instead of throwing it into pi.
  const on = (name: string, handler: (event: any, ctx: any) => Promise<any>) =>
    pi.on(name, async (event: any, ctx: any) => {
      try {
        return await handler(event, ctx)
      } catch (e) {
        logError(`${name} handler failed`, e)
      }
    })

  // New, resumed or forked session: brief again on its first run.
  on("session_start", async () => {
    briefed = false
    lastMessage = ""
  })

  // Before a run: load the resume brief once per session, then journal the prompt.
  on("before_agent_start", async (event: any, ctx: any) => {
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
  on("tool_result", async (event: any, ctx: any) => {
    if (event.isError) return
    const input = event.input ?? {}
    let call
    if (event.toolName === "write" || event.toolName === "edit") call = { tool_name: "Edit", tool_input: { file_path: input.path } }
    else if (event.toolName === "bash") call = { tool_name: "Bash", tool_input: { command: input.command } }
    if (call) await memdusa("post-tool-use", { session_id: sessionOf(ctx), cwd: ctx.cwd, ...call })
  })

  // The run ended: remember its last assistant message (verified on pi 0.73.1: `agent_end` comes first,
  // `agent_before_settle` right after) so the stop below can journal it.
  on("agent_end", async (event: any) => {
    lastMessage = lastAssistantText(event.messages)
  })

  // Last chance before pi settles: journal the agent's last message, and ask it to checkpoint if it changed files
  // (memdusa allows this at most once per session per window, so it can't loop).
  on("agent_before_settle", async (event: any, ctx: any) => {
    const out = await memdusa("stop", {
      session_id: sessionOf(ctx),
      cwd: ctx.cwd,
      stop_hook_active: false,
      ...(lastMessage ? { last_assistant_message: lastMessage } : {}),
    })
    if (out?.decision !== "block" || !out.reason) return
    return {
      entries: [...(event.entries ?? []), { type: "custom_message", customType: "memdusa", content: out.reason, display: false }],
      continue: true,
    }
  })

  // Context is about to be lost: write a fallback checkpoint if the agent changed files without one.
  on("session_before_compact", async (event: any, ctx: any) => {
    await memdusa("pre-compact", { session_id: sessionOf(ctx), cwd: ctx.cwd, trigger: event.reason === "manual" ? "manual" : "auto" })
  })

  on("session_shutdown", async (event: any, ctx: any) => {
    await memdusa("session-end", { session_id: sessionOf(ctx), cwd: ctx.cwd, reason: event.reason ?? "other" })
  })
}
