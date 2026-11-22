// memdusa for OpenCode (v2 plugin API). Written by `memdusa install opencode`;
// re-run that command instead of editing, and `memdusa uninstall opencode` to remove.
//
// Every event is handed to `memdusa hook` in memdusa's canonical format, so the
// offline queue, redaction, remote mode and code state work exactly as in other
// harnesses. The plugin only translates OpenCode's events.
import { Plugin } from "@opencode/plugin"
import { spawn } from "node:child_process"
import { appendFileSync, mkdirSync } from "node:fs"
import { homedir } from "node:os"
import { join } from "node:path"

const BIN: string = __MEMDUSA_BIN__
const AGENT = "opencode"

/** One line in `$MEMDUSA_HOME/logs/plugin.log` (default `~/.memdusa/logs`), so failures are visible. Never throws. */
function logError(what: string, err?: unknown) {
  try {
    const dir = join(process.env.MEMDUSA_HOME || join(homedir(), ".memdusa"), "logs")
    mkdirSync(dir, { recursive: true })
    const detail = err instanceof Error ? err.message : err === undefined ? "" : String(err)
    appendFileSync(join(dir, "plugin.log"), `${new Date().toISOString()} ${AGENT}: ${what}${detail ? `: ${detail}` : ""}\n`)
  } catch {}
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
      const child = spawn(BIN, ["hook", event, "--agent", AGENT__MEMDUSA_EXTRA_ARGS__], { stdio: ["pipe", "pipe", "ignore"] })
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

/** Run a hook body; a failure is logged, never thrown into OpenCode. */
async function guarded(name: string, body: () => Promise<void>) {
  try {
    await body()
  } catch (e) {
    logError(`${name} handler failed`, e)
  }
}

/** The text of the newest assistant message in a `session.context` result. */
function lastAssistantText(messages: any): string {
  if (!Array.isArray(messages)) return ""
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]
    if (m?.type !== "assistant") continue
    const text = (Array.isArray(m.content) ? m.content : [])
      .filter((p: any) => p?.type === "text" && typeof p.text === "string")
      .map((p: any) => p.text)
      .join("\n")
      .trim()
    return text // the newest assistant message only: older ones are not the last words
  }
  return ""
}

/** Files named in an apply_patch-style patch. */
function patchFiles(patch: string): string[] {
  const files = new Set<string>()
  for (const line of patch.split("\n")) {
    const m = line.match(/^\*\*\* (?:Add|Update|Delete) File: (.+)$|^\*\*\* Move to: (.+)$/)
    const path = (m?.[1] ?? m?.[2])?.trim()
    if (path) files.add(path)
  }
  return [...files]
}

export default Plugin.define({
  id: "memdusa",
  async setup(ctx) {
    const cwd = ctx.location.directory
    const briefed = new Set<string>()

    // First prompt of a session (new or resumed): load the resume brief as session context.
    // Every prompt: journal what the user asked for.
    await ctx.session.hook("prompt", (event: any) =>
      guarded("prompt", async () => {
        const sessionID = event.sessionID
        if (!briefed.has(sessionID)) {
          briefed.add(sessionID)
          const out = await memdusa("session-start", { session_id: sessionID, cwd, source: "startup" })
          const brief = out?.hookSpecificOutput?.additionalContext
          if (brief) await ctx.session.instructions.entry.put({ sessionID, key: "memdusa", value: brief })
        }
        const recall = await memdusa("user-prompt-submit", { session_id: sessionID, cwd, prompt: event.prompt?.text ?? "" })
        // Memories relevant to this prompt (none when nothing matches or recall is off).
        const relevant = recall?.hookSpecificOutput?.additionalContext
        if (relevant) await ctx.session.instructions.entry.put({ sessionID, key: "memdusa-recall", value: relevant })
      }),
    )

    // File edits and shell commands.
    await ctx.tool.hook("execute.after", (event: any) =>
      guarded("execute.after", async () => {
        if (event.status !== "completed") return
        const input = event.input ?? {}
        let call: { tool_name: string; tool_input: Record<string, unknown> } | undefined
        if (event.tool === "edit" || event.tool === "write") call = { tool_name: "Edit", tool_input: { file_path: input.path } }
        else if (event.tool === "patch") call = { tool_name: "Edit", tool_input: { file_paths: patchFiles(input.patchText ?? "") } }
        else if (event.tool === "shell") call = { tool_name: "Bash", tool_input: { command: input.command } }
        if (call) await memdusa("post-tool-use", { session_id: event.sessionID, cwd, ...call })
      }),
    )

    // Before compaction: write a fallback checkpoint if the agent changed files without one.
    await ctx.session.hook("compaction", (event: any) =>
      guarded("compaction", async () => {
        await memdusa("pre-compact", { session_id: event.sessionID, cwd, trigger: "auto" })
      }),
    )

    // When the agent goes idle (verified on OpenCode 2.2.6: payload `{ sessionID, background: [] }`, fired once
    // per finished turn): journal its last message and ask it to checkpoint if it changed files (at most once
    // per window). `background` lists still-running background tasks, and `[]` is truthy: test its length.
    // OpenCode has no session-end hook, so no `session-end` event is sent.
    try {
      await ctx.session.hook("experimental.idle" as any, (event: any) =>
        guarded("experimental.idle", async () => {
          if (event.background?.length) return
          let last = ""
          try {
            last = lastAssistantText(await ctx.session.context({ sessionID: event.sessionID }))
          } catch (e) {
            logError("could not read the session's last message", e)
          }
          const out = await memdusa("stop", {
            session_id: event.sessionID,
            cwd,
            stop_hook_active: false,
            ...(last ? { last_assistant_message: last } : {}),
          })
          if (out?.decision === "block" && out.reason) await ctx.session.synthetic({ sessionID: event.sessionID, text: out.reason })
        }),
      )
    } catch (e) {
      logError("could not register the idle hook (experimental.idle)", e)
    }
  },
})
