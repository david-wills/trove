import type { HealthSource } from "../api";
import { SOURCE_META } from "./healthShared";

/** Multi-select of sources. At least one stays on: hiding every source is
 *  never what anyone means. */
export default function SourceChips({
  available,
  selected,
  onChange,
  small,
}: {
  available: HealthSource[];
  selected: HealthSource[];
  onChange: (next: HealthSource[]) => void;
  small?: boolean;
}) {
  const toggle = (s: HealthSource) => {
    const on = selected.includes(s);
    if (on && selected.length === 1) return;
    onChange(on ? selected.filter((x) => x !== s) : available.filter((x) => x === s || selected.includes(x)));
  };
  return (
    <div className={`source-chips ${small ? "source-chips--sm" : ""}`}>
      {available.map((s) => {
        const on = selected.includes(s);
        return (
          <button
            key={s}
            className={`source-chip-btn ${on ? "on" : ""}`}
            style={on ? { borderColor: SOURCE_META[s].color, color: SOURCE_META[s].color } : undefined}
            onClick={() => toggle(s)}
            title={on ? `Hide ${SOURCE_META[s].label}` : `Show ${SOURCE_META[s].label}`}
          >
            <span className="source-dot" style={{ background: on ? SOURCE_META[s].color : "var(--text-faint)" }} />
            {SOURCE_META[s].label}
          </button>
        );
      })}
    </div>
  );
}
