#!/usr/bin/env node
// Renders the integrations-research workflow output into a markdown report.
import { readFileSync, writeFileSync } from 'node:fs';

const SRC = process.argv[2];
const OUT = process.argv[3];
const raw = JSON.parse(readFileSync(SRC, 'utf8'));
const { domains, synthesis } = raw.result;

const cell = (s) => String(s ?? '').replace(/\|/g, '\\|').replace(/\n+/g, ' ').trim();
const slug = (s) => s.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '');

// Feasibility marker
const mark = (f) => {
  const t = (f || '').toLowerCase();
  if (t.startsWith('high')) return '🟢';
  if (t.startsWith('medium')) return '🟡';
  if (t.startsWith('low')) return '🟠';
  if (t.startsWith('blocked')) return '🔴';
  return '⚪';
};
const statusBadge = (s) => ({ built: '✅ built', planned: '📋 planned', new: '🆕 new' }[s] || s);

const totalSources = domains.reduce((n, d) => n + (d.sources?.length || 0), 0);

let o = '';
o += `# Trove — Integrations Research\n\n`;
o += `*Generated 2026-06-11 by a 17-domain parallel research sweep (18 agents, ~1.1M tokens). `;
o += `An exhaustive feasibility map of every data source Trove could collect, judged against the hard `;
o += `constraints: local-first, files-as-truth, **standalone** (no runtime dependency on an external app/service), `;
o += `built-for-anyone, macOS. Companion to \`docs/data-sources.md\` (the build checklist) — this is the wide net; `;
o += `that is the working order.*\n\n`;

o += `**${totalSources} sources** researched across **${domains.length} domains**. `;
o += `Feasibility: 🟢 High · 🟡 Medium · 🟠 Low · 🔴 Blocked. Status: ✅ built · 📋 planned · 🆕 new.\n\n`;

o += `**Mechanisms:** M1 one-shot file import · M2 watch folder · M3 local DB copy-then-read (often needs Full Disk Access) · `;
o += `M4 OS API watcher (TCC) · M5 cloud API pull (OAuth/token) · M6 agent collector.\n\n`;

o += `---\n\n## Contents\n\n`;
o += `1. [Executive Summary](#executive-summary)\n`;
o += `2. [Top Recommendations (prioritized)](#top-recommendations)\n`;
o += `3. [Reusable Mechanism Investments](#reusable-mechanism-investments)\n`;
o += `4. [Aggregator Hubs](#aggregator-hubs)\n`;
o += `5. [Hard Blocks](#hard-blocks)\n`;
o += `6. [Cross-Cutting Themes](#cross-cutting-themes)\n`;
o += `7. Domain catalogs:\n`;
domains.forEach((d, i) => { o += `   ${i + 1}. [${d.domain}](#${slug(d.domain)}) — ${d.sources?.length || 0} sources\n`; });
o += `\n---\n\n`;

// ---- Synthesis ----
o += `## Executive Summary\n\n${synthesis.executive_summary}\n\n`;

o += `## Top Recommendations\n\n`;
o += `Highest-value **net-new** sources to build next, weighing value × feasibility × time-sensitivity.\n\n`;
o += `| Priority | Source | Domain | Effort | Why |\n|---|---|---|---|---|\n`;
const prioOrder = { P0: 0, P1: 1, P2: 2 };
[...synthesis.top_recommendations]
  .sort((a, b) => (prioOrder[a.priority] ?? 9) - (prioOrder[b.priority] ?? 9))
  .forEach((r) => {
    o += `| **${cell(r.priority)}** | ${cell(r.name)} | ${cell(r.domain)} | ${cell(r.effort)} | ${cell(r.why)} |\n`;
  });
o += `\n`;

o += `## Reusable Mechanism Investments\n\n${synthesis.reusable_mechanisms}\n\n`;
o += `## Aggregator Hubs\n\n${synthesis.aggregator_hubs}\n\n`;
o += `## Hard Blocks\n\n${synthesis.hard_blocks}\n\n`;
o += `## Cross-Cutting Themes\n\n${synthesis.thematic_observations}\n\n`;

// ---- Domain catalogs ----
o += `---\n\n# Domain Catalogs\n\n`;
for (const d of domains) {
  o += `## ${d.domain}\n\n${d.overview}\n\n`;

  // At-a-glance table
  o += `### At a glance\n\n`;
  o += `| Source | Subcategory | Mech | Permission | Effort | Feasibility | Status |\n`;
  o += `|---|---|---|---|---|---|---|\n`;
  for (const s of d.sources || []) {
    o += `| ${cell(s.name)} | ${cell(s.subcategory)} | ${cell(s.mechanism)} | ${cell(s.permissions)} | ${cell(s.effort)} | ${mark(s.feasibility)} ${cell(s.feasibility)} | ${statusBadge(s.status_in_trove)} |\n`;
  }
  o += `\n### Detail\n\n`;
  for (const s of d.sources || []) {
    o += `#### ${s.name} — _${s.subcategory}_\n\n`;
    o += `${mark(s.feasibility)} **${cell(s.feasibility)}** · ${cell(s.mechanism)} · ${cell(s.permissions)} · effort **${cell(s.effort)}** · ${statusBadge(s.status_in_trove)}\n\n`;
    if (s.access) o += `- **Access:** ${s.access}\n`;
    if (s.recommendation) o += `- **Recommendation:** ${s.recommendation}\n`;
    if (s.notes) o += `- **Notes:** ${s.notes}\n`;
    o += `\n`;
  }
  if (d.cross_cutting) o += `### ${d.domain} — cross-cutting notes\n\n${d.cross_cutting}\n\n`;
  o += `---\n\n`;
}

writeFileSync(OUT, o);
console.log(`Wrote ${OUT}: ${o.length} chars, ${o.split('\n').length} lines, ${totalSources} sources, ${domains.length} domains`);
