// Sidebar order and last-open section. Both are user preferences, not
// vault data, so they live in localStorage rather than in ~/Documents/Trove.

export const NAV_ORDER_KEY = "trove.nav-order";
export const SECTION_KEY = "trove.section";

/// Saved order first (unknown ids dropped), then any ids the saved list
/// doesn't know about, in default order — so a new section appears at the
/// end instead of vanishing.
export function mergeOrder<T extends string>(defaults: readonly T[], saved: unknown): T[] {
  if (!Array.isArray(saved)) return [...defaults];
  const known = new Set<string>(defaults);
  const kept = saved.filter((id): id is T => typeof id === "string" && known.has(id));
  const seen = new Set(kept);
  return [...kept, ...defaults.filter((id) => !seen.has(id))];
}

export function loadOrder<T extends string>(defaults: readonly T[]): T[] {
  try {
    return mergeOrder(defaults, JSON.parse(localStorage.getItem(NAV_ORDER_KEY) ?? "[]"));
  } catch {
    return [...defaults];
  }
}

export function saveOrder(order: readonly string[]): void {
  localStorage.setItem(NAV_ORDER_KEY, JSON.stringify(order));
}

/// Which slot a drag that started at `from` and travelled `dy` px lands in,
/// given equal `step`-px slots and `count` items.
export function dropIndex(from: number, dy: number, step: number, count: number): number {
  return Math.max(0, Math.min(count - 1, Math.round(from + dy / step)));
}

export function moveItem<T>(list: readonly T[], from: number, to: number): T[] {
  const next = [...list];
  const [item] = next.splice(from, 1);
  next.splice(to, 0, item);
  return next;
}
