import { useEffect, useState } from "react";
import { api, HealthSource, WorkoutItem } from "../api";
import { fmtDay, fmtTime, SOURCE_META, titleCase } from "./healthShared";

/** Workouts and sessions from every enabled source, newest first. */
export default function WorkoutsPane({ sources }: { sources: HealthSource[] }) {
  const [items, setItems] = useState<WorkoutItem[] | null>(null);

  useEffect(() => {
    api.healthWorkouts(300).then(setItems).catch(() => setItems([]));
  }, []);

  const shown = (items ?? []).filter((w) => sources.includes(w.source));

  return (
    <>
      <div className="view-header">
        <div>
          <h2>Workouts</h2>
          <div className="health-header-sub">
            {items === null ? "" : `${shown.length} newest · ${sources.map((s) => SOURCE_META[s].label).join(", ")}`}
          </div>
        </div>
      </div>
      {items !== null && shown.length === 0 && (
        <div className="oura-hint">
          No workouts from the enabled sources — they arrive with an Apple Health import or an Oura Ring sync.
        </div>
      )}
      <div className="workout-list">
        {shown.map((w, i, all) => {
          const dayHead = i === 0 || w.day !== all[i - 1].day ? fmtDay(w.day) : null;
          return (
            <div key={`${w.source}-${w.start}-${i}`}>
              {dayHead && <div className="tl-day">{dayHead}</div>}
              <div className="workout-row">
                <span className="workout-time">{fmtTime(w.start)}</span>
                <span className="workout-activity">
                  {titleCase(w.activity)}
                  {w.label && <span className="workout-label"> · {w.label}</span>}
                </span>
                {w.kind === "session" && <span className="kind-chip">Session</span>}
                <span className="source-chip" style={{ color: SOURCE_META[w.source].color }}>
                  {SOURCE_META[w.source].label}
                </span>
                <span className="workout-stats">
                  {w.duration_min != null && `${Math.round(w.duration_min)} min`}
                  {w.calories != null && ` · ${Math.round(w.calories)} kcal`}
                  {w.distance_km != null && w.distance_km > 0 && ` · ${w.distance_km.toFixed(1)} km`}
                  {w.intensity && ` · ${w.intensity}`}
                </span>
              </div>
            </div>
          );
        })}
      </div>
    </>
  );
}
