// memdusa for OpenCode (v2 plugin API). Written by `memdusa install opencode`;
// re-run that command instead of editing, and `memdusa uninstall opencode` to remove.
//
// Every event is handed to `memdusa hook` in memdusa's canonical format, so the
// offline queue, redaction, remote mode and code state work exactly as in other
// harnesses. The plugin only translates OpenCode's events.
import { Plugin } from "@opencode/plugin"
import { spawn } from "node:child_process"

const BIN: string = __MEMDUSA_BIN__
const AGENT = "opencode"

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
    await ctx.session.hook("prompt", async (event: any) => {
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
    })

    // File edits and shell commands.
    await ctx.tool.hook("execute.after", async (event: any) => {
      if (event.status !== "completed") return
      const input = event.input ?? {}
      let call: { tool_name: string; tool_input: Record<string, unknown> } | undefined
      if (event.tool === "edit" || event.tool === "write") call = { tool_name: "Edit", tool_input: { file_path: input.path } }
      else if (event.tool === "patch") call = { tool_name: "Edit", tool_input: { file_paths: patchFiles(input.patchText ?? "") } }
      else if (event.tool === "shell") call = { tool_name: "Bash", tool_input: { command: input.command } }
      if (call) await memdusa("post-tool-use", { session_id: event.sessionID, cwd, ...call })
    })

    // Before compaction: write a fallback checkpoint if the agent changed files without one.
    await ctx.session.hook("compaction", async (event: any) => {
      await memdusa("pre-compact", { session_id: event.sessionID, cwd, trigger: "auto" })
    })

    // When the agent goes idle: ask it to checkpoint if it changed files (at most once per window).
    // `experimental.idle` may not exist in every OpenCode version.
    try {
      await ctx.session.hook("experimental.idle" as any, async (event: any) => {
        if (event.background) return
        const out = await memdusa("stop", { session_id: event.sessionID, cwd, stop_hook_active: false })
        if (out?.decision === "block" && out.reason) await ctx.session.synthetic({ sessionID: event.sessionID, text: out.reason })
      })
    } catch {}
  },
})
