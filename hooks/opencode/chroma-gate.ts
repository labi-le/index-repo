import type { Plugin } from "@opencode-ai/plugin"

// Collection name mirrors the index-repo indexer exactly: `code-<basename(root)>`
// where `root` is the launch cwd ($PWD) the indexer was pointed at. See
// src/main.rs / src/service.rs:  format!("code-{}", root.file_name()…unwrap_or("unknown"))
// Computed once at plugin load; opencode loads plugins with cwd == workspace root.
const cwd = process.cwd()
const COLLECTION = `code-${cwd.split("/").filter(Boolean).pop() ?? "unknown"}`

const CHROMA_SEARCH_RULE = [
  `MANDATORY codebase search: call chroma_query_documents FIRST. Collection: ${COLLECTION} (code-<basename of workspace root>).`,
  "Use grep/glob only after a chroma call, when you already know the exact file path, or when chroma is empty/missing.",
  "Never bypass chroma with bash search (rg/grep/find). 'I'll just grep quickly' / 'too simple for search' are not valid reasons — chroma first.",
  "Enforced by the chroma-gate plugin: grep/glob without a prior chroma call are blocked.",
].join("\n")

const chromaCalled = new Set<string>()
const sessionAgent = new Map<string, string>()

const ENFORCED_AGENTS = new Set<string>([
  "build",
  "orchestrator",
  "general",
  "explore",
  "explorer",
  "plan",
])

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

    const agent = sessionAgent.get(sessionID)
    if (!agent || !ENFORCED_AGENTS.has(agent)) return

    if (chromaCalled.has(sessionID)) return

    const args = output?.args
    if (isGrep(tool) && isNarrowedGrep(args)) return
    if (isGlob(tool) && isNarrowedGlob(args)) return

    throw new Error(blockMessage(tool))
  },
})) satisfies Plugin
