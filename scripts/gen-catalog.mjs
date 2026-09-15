#!/usr/bin/env node
// Renders docs/integrations/INDEX.md into src/generated/catalog.json so the
// app can show every source ever briefed — including the ones pruned from
// this build — without shipping the docs folder. Runs before `vite` via the
// predev/prebuild npm hooks; the output is committed so a clean checkout
// builds without running it.
import { readFileSync, writeFileSync, existsSync, mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const INDEX = join(root, "docs/integrations/INDEX.md");
const OUT = join(root, "src/generated/catalog.json");
const RESTORE_COMMIT = "33bda15";

const STATUS = { "🧪": "built", "📦": "pruned", "📋": "queued", "🚫": "unavailable", "🚧": "building", "✅": "validated" };
const ROW = /^\|\s*(?:[0-9]+|—)\s*\|\s*\[`([^`]+)`\]\(\.\/([^)]+)\)\s*\|\s*([^|]+?)\s*\|\s*(\S+)\s*\|\s*`?([^|`]*)`?\s*\|\s*([^|]*?)\s*\|\s*([^|]*?)\s*\|\s*([^|]*?)\s*\|\s*(.*?)\s*\|\s*$/;

const rows = [];
for (const line of readFileSync(INDEX, "utf8").split("\n")) {
  const m = ROW.exec(line);
  if (!m) continue;
  const [, slug, brief, name, mark, domain, priority, effort, needs, summary] = m;
  const briefPath = join(root, "docs/integrations", brief);
  let modules = [];
  let reason = "";
  if (existsSync(briefPath)) {
    const text = readFileSync(briefPath, "utf8");
    modules = [...new Set([...text.matchAll(/crates\/trove-core\/src\/([a-z0-9_/]+\.rs)/g)].map((x) => x[1]))];
    const r = /^- \*\*unavailable_reason:\*\*\s*(.+)$/m.exec(text);
    if (r && !/^none\b/i.test(r[1].trim())) reason = r[1].trim();
  }
  rows.push({
    id: slug,
    name: name.trim(),
    status: STATUS[mark] ?? mark,
    domain: domain.trim().replace(/\/$/, ""),
    priority: priority.trim(),
    effort: effort.trim(),
    needs: needs.trim() === "—" ? "" : needs.trim(),
    summary: summary.trim(),
    brief: `docs/integrations/${brief}`,
    modules,
    unavailable_reason: reason,
    restore_commit: STATUS[mark] === "pruned" ? RESTORE_COMMIT : "",
  });
}
mkdirSync(dirname(OUT), { recursive: true });
writeFileSync(OUT, JSON.stringify({ generated_from: "docs/integrations/INDEX.md", rows }, null, 1) + "\n");
const counts = rows.reduce((a, r) => ((a[r.status] = (a[r.status] ?? 0) + 1), a), {});
console.log(`catalog: ${rows.length} rows`, counts);
