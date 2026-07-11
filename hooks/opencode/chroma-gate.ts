import type { Plugin } from "@opencode-ai/plugin"
import { createHash } from "node:crypto"
import { execFileSync } from "node:child_process"
import { realpathSync } from "node:fs"

// Collection name mirrors src/config.rs::collection_name EXACTLY:
//   1. git identity → code-<owner>-<repo>[-<subpath>]   (machine-stable)
//   2. fallback     → code-<basename>-<hash8>           (sha1 of canonical path)
// opencode loads plugins with cwd == workspace root.
const ROOT = (() => {
  try {
    return realpathSync(process.cwd())
  } catch {
    return process.cwd()
  }
})()

function git(args: string[]): string | null {
  try {
    const out = execFileSync("git", ["-C", ROOT, ...args], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    }).trim()
    return out.length > 0 ? out : null
  } catch {
    return null
  }
}

function normalizeRemote(url: string): string | null {
  const s = url.trim().replace(/^git\+/, "")
  let hostpath: string
  const scheme = s.indexOf("://")
  if (scheme >= 0) {
    const rest = s.slice(scheme + 3)
    const at = rest.indexOf("@")
    hostpath = at >= 0 ? rest.slice(at + 1) : rest
  } else {
    const colon = s.indexOf(":")
    if (colon >= 0) {
      const before = s.slice(0, colon)
      const host = before.includes("@") ? before.slice(before.lastIndexOf("@") + 1) : before
      hostpath = `${host}/${s.slice(colon + 1)}`
    } else {
      hostpath = s
    }
  }
  hostpath = hostpath.replace(/\/+$/, "").replace(/\.git$/, "")
  const slash = hostpath.indexOf("/")
  const path = (slash >= 0 ? hostpath.slice(slash + 1) : hostpath).replace(/^\/+|\/+$/g, "")
  return path.length > 0 ? path : null
}

function sanitizeSlug(raw: string): string {
  let out = ""
  let prevDash = false
  for (const ch of raw) {
    const keep = /[A-Za-z0-9._-]/.test(ch)
    const c = keep ? ch.toLowerCase() : "-"
    if (c === "-") {
      if (prevDash) continue
      prevDash = true
    } else {
      prevDash = false
    }
    out += c
  }
  return out.replace(/^-+|-+$/g, "")
}

function finalizeCollection(raw: string): string {
  const s = sanitizeSlug(raw)
  const name = `code-${s}`
  if (name.length <= 63) return name
  const h = createHash("sha1").update(raw).digest("hex").slice(0, 8)
  return `code-${s.slice(0, 49)}-${h}`
}

function resolveCollection(): string {
  const toplevel = git(["rev-parse", "--show-toplevel"])
  const url = toplevel ? git(["config", "--get", "remote.origin.url"]) : null
  const path = url ? normalizeRemote(url) : null
  if (toplevel && path) {
    const rel =
      ROOT === toplevel ? "" : ROOT.startsWith(`${toplevel}/`) ? ROOT.slice(toplevel.length + 1) : ""
    return finalizeCollection(rel ? `${path}/${rel}` : path)
  }
  const base = ROOT.split("/").filter(Boolean).pop() ?? "unknown"
  const hash8 = createHash("sha1").update(ROOT).digest("hex").slice(0, 8)
  return finalizeCollection(`${base}-${hash8}`)
}

const COLLECTION = resolveCollection()

const CHROMA_SEARCH_RULE = [
  `MANDATORY codebase search: call chroma_query_documents FIRST. Collection: ${COLLECTION}.`,
  "Use grep/glob only after a chroma call, when you already know the exact file path, or when chroma is empty/missing.",
  "Never bypass chroma with bash search (rg/grep/find). 'I'll just grep quickly' / 'too simple for search' are not valid reasons — chroma first.",
  "Enforced by the chroma-gate plugin: grep/glob without a prior chroma call are blocked.",
].join("\n")

const chromaCalled = new Set<string>()
const sessionAgent = new Map<string, string>()

// Enforcement is configurable: CHROMA_GATE_ENFORCE=0|false|no|off disables the
// grep/glob block (the system-rule hint still injects); CHROMA_GATE_AGENTS is a
// comma-separated allowlist that overrides the default enforced-agent set.
const ENFORCE = !/^(0|false|no|off)$/i.test((process.env.CHROMA_GATE_ENFORCE ?? "").trim())
const AGENTS_OVERRIDE = (process.env.CHROMA_GATE_AGENTS ?? "")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean)
const ENFORCED_LIST =
  AGENTS_OVERRIDE.length > 0
    ? AGENTS_OVERRIDE
    : ["build", "orchestrator", "general", "explore", "explorer", "plan"]
const ENFORCED_AGENTS: Record<string, true> = Object.fromEntries(
  ENFORCED_LIST.map((a) => [a, true] as const),
)

const isChromaQuery = (name: string) =>
  name === "chroma_query_documents" ||
  name.endsWith("_chroma_query_documents") ||
  (name.toLowerCase().includes("chroma") && name.toLowerCase().includes("query"))

const isGrep = (name: string) => name === "grep" || name.endsWith("_grep")
const isGlob = (name: string) => name === "glob" || name.endsWith("_glob")

const isNarrowedGrep = (args: any) => {
  if (!args || typeof args !== "object") return false
  const hasInclude = typeof args.include === "string" && args.include.length > 0
  const path = typeof args.path === "string" ? args.path : ""
  const hasConcretePath =
    path.length > 0 && path !== "." && path !== "/" && !path.endsWith("/**")
  return hasInclude && hasConcretePath
}

const isNarrowedGlob = (args: any) => {
  if (!args || typeof args !== "object") return false
  const path = typeof args.path === "string" ? args.path : ""
  const pattern = typeof args.pattern === "string" ? args.pattern : ""
  if (path.length > 0 && path !== "." && path !== "/") return true
  if (pattern && !pattern.startsWith("**") && !pattern.startsWith("/**")) return true
  return false
}

const rememberAgent = (sessionID: unknown, agent: unknown) => {
  const sid = typeof sessionID === "string" ? sessionID : ""
  const ag = typeof agent === "string" ? agent : ""
  if (sid && ag) sessionAgent.set(sid, ag)
}

const blockMessage = (tool: string) =>
  [
    "BLOCKED by chroma-gate: " + tool + " requires a prior chroma_query_documents call in this session.",
    "Action: call chroma_query_documents first (collection name: " + COLLECTION + ").",
    "Exception: " + tool + " is allowed without chroma only when narrowed by both args.path AND args.include (grep) or args.path/args.pattern targeting a specific subtree (glob).",
  ].join(" ")

export default (async () => ({
  "experimental.chat.system.transform": async (_input, output) => {
    if (!Array.isArray(output.system) || output.system.length === 0) return
    if (!output.system.includes(CHROMA_SEARCH_RULE)) {
      output.system.splice(1, 0, CHROMA_SEARCH_RULE)
    }
  },
  "chat.message": async (input) => {
    rememberAgent(input.sessionID, (input as any).agent)
  },
  "chat.params": async (input) => {
    rememberAgent(input.sessionID, (input as any).agent)
  },
  "tool.execute.before": async (input, output) => {
    const tool = String(input.tool ?? "")
    const sessionID = String(input.sessionID ?? "")

    if (isChromaQuery(tool)) {
      if (sessionID) chromaCalled.add(sessionID)
      return
    }

    if (!isGrep(tool) && !isGlob(tool)) return
    if (!ENFORCE) return // blocking disabled via CHROMA_GATE_ENFORCE

    const agent = sessionAgent.get(sessionID)
    if (!agent || ENFORCED_AGENTS[agent] !== true) return

    if (chromaCalled.has(sessionID)) return

    const args = output?.args
    if (isGrep(tool) && isNarrowedGrep(args)) return
    if (isGlob(tool) && isNarrowedGlob(args)) return

    throw new Error(blockMessage(tool))
  },
})) satisfies Plugin
