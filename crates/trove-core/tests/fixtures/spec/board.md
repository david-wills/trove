---
title: Sleep × Calendar
panels:
- title: Today
  kind: tile
  bucket: day
  days: 7
  series:
  - metric: readiness-score
    agg: avg
  - metric: sleep-score
    agg: avg
- title: Sleep score vs meetings
  kind: dual
  bucket: day
  days: 90
  series:
  - metric: sleep-score
    source: oura
    agg: avg
    label: Sleep score
  - table: calendar/events
    column: '@records'
    agg: sum
    label: Events
- title: Hours asleep per week
  kind: bars
  bucket: week
  days: 180
  series:
  - table: health/sleep/oura
    column: asleep_seconds
    agg: sum
    divide: 3600.0
    unit: h
---

Does a packed calendar cost sleep?
