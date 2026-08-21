import { execFile, spawnSync } from "node:child_process";
import { promisify } from "node:util";
import { existsSync } from "node:fs";
import { join } from "node:path";

const run = promisify(execFile);
const INDEX_REPO = "@index_repo_bin@";

async function ensureRegistered(ctx) {
  if (process.env.CODE_INDEXER_ACTIVE || process.env.CODE_INDEXER_DISABLE) return;
  const cwd = (ctx && ctx.cwd) || process.cwd();
  if (!cwd || !existsSync(join(cwd, ".git")) || existsSync(join(cwd, ".no-code-index"))) return;
  process.env.CODE_INDEXER_ACTIVE = "1";
  try { await run("systemctl", ["--user", "start", "--no-block", "index-repo.service"]); } catch {}
  try { await run(INDEX_REPO, ["register", cwd, "--pid", String(process.pid)]); } catch {}
  process.once("exit", () => {
    try { spawnSync(INDEX_REPO, ["unregister", cwd, "--pid", String(process.pid)]); } catch {}
  });
}

export default function (pi) {
  pi.on("session_start", (_event, ctx) => ensureRegistered(ctx));
  pi.on("agent_start", (_event, ctx) => ensureRegistered(ctx));
}
