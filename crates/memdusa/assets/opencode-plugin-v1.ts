// memdusa for OpenCode (v1 plugin API, OpenCode 1.x). Written by `memdusa install opencode`;
// re-run that command instead of editing, and `memdusa uninstall opencode` to remove.
//
// Every event is handed to `memdusa hook` in memdusa's canonical format, so the
// offline queue, redaction, remote mode and code state work exactly as in other
// harnesses. The plugin only translates OpenCode's events. It imports nothing from
// OpenCode, so it loads without the plugin package being installed.
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

/** The environment for `memdusa hook`; a stale MEMDUSA_PROJECT would file the session under the wrong project. */
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

/** Run a hook body; a failure is logged, never thrown into OpenCode. */
async function guarded(name: string, body: () => Promise<void>) {
  try {
    await body()
  } catch (e) {
    logError(`${name} handler failed`, e)
  }
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

/** A finished tool call in memdusa's canonical shape (or undefined for tools memdusa doesn't journal). */
function canonicalCall(tool: string, args: any): { tool_name: string; tool_input: Record<string, unknown> } | undefined {
  args = args ?? {}
  const file = args.filePath ?? args.file_path ?? args.path
  if (tool === "write" || tool === "edit" || tool === "multiedit") return file ? { tool_name: "Edit", tool_input: { file_path: file } } : undefined
  if (tool === "patch" || tool === "apply_patch") return { tool_name: "Edit", tool_input: { file_paths: patchFiles(args.patchText ?? args.patch ?? "") } }
  if (tool === "bash" || tool === "shell") return args.command ? { tool_name: "Bash", tool_input: { command: args.command } } : undefined
  return undefined
}

/** The text of the newest assistant message in a `session.messages` result (`[{ info, parts }]`). */
function lastAssistantText(messages: any): string {
  if (!Array.isArray(messages)) return ""
  for (let i = messages.length - 1; i >= 0; i--) {
    const m = messages[i]
    if (m?.info?.role !== "assistant") continue
    return (Array.isArray(m.parts) ? m.parts : [])
      .filter((p: any) => p?.type === "text" && typeof p.text === "string" && !p.synthetic)
      .map((p: any) => p.text)
      .join("\n")
      .trim()
  }
  return ""
}

/** The user's prompt text from a `chat.message` hook's parts. */
function promptText(parts: any): string {
  return (Array.isArray(parts) ? parts : [])
    .filter((p: any) => p?.type === "text" && typeof p.text === "string" && !p.synthetic)
    .map((p: any) => p.text)
    .join("\n")
    .trim()
}

export const MemdusaPlugin = async ({ client, directory }: any) => {
  const cwd: string = directory
  const briefed = new Set<string>()
  // Context for each session's model calls: the resume brief (once per session) and the latest prompt's recall.
  const brief = new Map<string, string>()
  const recall = new Map<string, string>()

  return {
    // First prompt of a session (new or resumed): load the resume brief. Every prompt: journal it and recall.
    "chat.message": (input: any, output: any) =>
      guarded("chat.message", async () => {
        const sessionID: string = input.sessionID
        if (!briefed.has(sessionID)) {
          briefed.add(sessionID)
          const out = await memdusa("session-start", { session_id: sessionID, cwd, source: "startup" })
          const text = out?.hookSpecificOutput?.additionalContext
          if (text) brief.set(sessionID, text)
        }
        const out = await memdusa("user-prompt-submit", { session_id: sessionID, cwd, prompt: promptText(output?.parts) })
        const relevant = out?.hookSpecificOutput?.additionalContext
        if (relevant) recall.set(sessionID, relevant)
        else recall.delete(sessionID)
      }),

    // Add the brief and recall to the system prompt of every model call in that session.
    "experimental.chat.system.transform": (input: any, output: any) =>
      guarded("experimental.chat.system.transform", async () => {
        const sessionID: string | undefined = input?.sessionID
        if (!sessionID || !Array.isArray(output?.system)) return
        for (const text of [brief.get(sessionID), recall.get(sessionID)]) if (text) output.system.push(text)
      }),

    // File edits and shell commands.
    "tool.execute.after": (input: any) =>
      guarded("tool.execute.after", async () => {
        const call = canonicalCall(input.tool, input.args)
        if (call) await memdusa("post-tool-use", { session_id: input.sessionID, cwd, ...call })
      }),

    // Before compaction: write a fallback checkpoint if the agent changed files without one, and keep the brief.
    "experimental.session.compacting": (input: any, output: any) =>
      guarded("experimental.session.compacting", async () => {
        await memdusa("pre-compact", { session_id: input.sessionID, cwd, trigger: "auto" })
        const text = brief.get(input.sessionID)
        if (text && Array.isArray(output?.context)) output.context.push(text)
      }),

    // When the agent goes idle: journal its last message and ask it to checkpoint if it changed files (at most
    // once per window). OpenCode has no session-end hook, so no `session-end` event is sent.
    event: ({ event }: any) =>
      guarded("event", async () => {
        if (event?.type !== "session.idle") return
        const sessionID: string | undefined = event.properties?.sessionID
        if (!sessionID) return
        let last = ""
        try {
          const res = await client.session.messages({ path: { id: sessionID } })
          last = lastAssistantText(res?.data ?? res)
        } catch (e) {
          logError("could not read the session's last message", e)
        }
        const out = await memdusa("stop", {
          session_id: sessionID,
          cwd,
          stop_hook_active: false,
          ...(last ? { last_assistant_message: last } : {}),
        })
        if (out?.decision === "block" && out.reason) {
          // A new turn asking the agent to checkpoint; not awaited, so the idle handler returns at once.
          client.session
            .prompt({ path: { id: sessionID }, body: { parts: [{ type: "text", text: out.reason }] } })
            .catch((e: unknown) => logError("could not send the checkpoint reminder", e))
        }
      }),
  }
}
