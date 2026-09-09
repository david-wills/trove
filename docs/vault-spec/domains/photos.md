# Domain: photos

Photo and video **metadata** — one record per asset, from every photo source:
the Mac's Apple Photos library, loose files via EXIF import, a Google Photos
Takeout, Flickr / SmugMug archives, a BeReal export, and the screenshot
watcher. **Metadata only, never image bytes** — the contract indexes when,
where, and on what a picture was taken; the pixels stay wherever the user
keeps them, and full source fidelity lives in each source's own raw folder.
Readers merge by scanning every source folder and sorting by `ts`; the same
asset reaching the vault through two sources (an Apple Photos library and a
Google Takeout of the same photo) writes two rows with two `guid`s and
reconciles at read time. GPS geotags make this a **privacy-sensitive**
location proxy: every source ships opt-in with explicit acknowledgement.

- **Layout:** `photos/<source>/YYYY-MM.jsonl` (month of `ts`)
- **Kind:** append-only event stream
- **Schema:** [`schemas/photos.photo.schema.json`](../schemas/photos.photo.schema.json)
- **Dedupe key:** `guid` (source-unique: Apple Photos asset UUID, an EXIF
  file's content hash, a Google sidecar URL, a Flickr/SmugMug image id, a
  BeReal post id, a screenshot's content hash). Re-imports of overlapping
  exports must skip already-stored guids before appending.

## Photo

One asset per line. Only `ts`, `source`, `guid` are required — a GPS-stripped
scan or a bare Takeout sidecar writes just those plus whatever it has, while a
rich library asset fills dimensions, GPS, camera, albums, and flags. Videos
and Live Photos carry `duration_secs`; screenshots carry their on-device OCR
`text`. Everything a source exposes that has no column here is preserved in
`extra` (and in full in the source's raw folder).

| Field | Type | Required | Meaning |
|---|---|---|---|
| `ts` | string | ✔ | RFC3339 local capture time (photo taken / video recorded / screenshot saved) |
| `source` | string | ✔ | collector id, = the folder name |
| `guid` | string | ✔ | source-unique id, the dedupe key |
| `kind` | string | | `"photo"` (default) \| `"video"` \| `"live"` \| `"screenshot"` \| `"burst"` |
| `filename` | string | | original file name (no path) |
| `title` | string | | caption / title where the source has one (Google description, Flickr/SmugMug caption) |
| `mime` | string | | MIME type (`"image/heic"`, `"image/jpeg"`, `"video/quicktime"`, …) |
| `width` | int | | pixel width |
| `height` | int | | pixel height |
| `lat` | number | | latitude, decimal degrees (WGS84) |
| `lon` | number | | longitude, decimal degrees (WGS84) |
| `camera_make` | string | | EXIF camera/device make (`"Apple"`, `"Canon"`, …) |
| `camera_model` | string | | EXIF camera/device model (`"iPhone 15 Pro"`, …) |
| `favorite` | bool | | flagged a favorite / faved on the source |
| `duration_secs` | number | | clip length for `"video"` / `"live"` assets |
| `albums` | string[] | | album / set names this asset belongs to (verbatim) |
| `tags` | string[] | | source-native tags / keywords (Flickr tags, SmugMug keywords, EXIF keywords) |
| `people` | string[] | | face/person cluster ids or labels (opt-in; raw source handles, not resolved across sources) |
| `people_name` | string[] | | display names, positionally paired with `people` where the source gives both |
| `text` | string | | extracted on-device OCR text (screenshots; never sent off-device) |
| `extra` | object | | everything source-specific (altitude, lens, orientation, file size, ML scene labels, view/fave counts, BeReal front/back file names, hidden flag, …) |

Omit empty fields. Unknown fields are tolerated.

Faces / people are **privacy-sensitive and opt-in** — off by default. Face
clusters and people tags (Apple Photos `ZPERSON`, Google sidecar `people[]`)
are recorded only when the user enables them, as raw source handles/labels in
`people` with any display names in the positionally-paired `people_name`
sibling; they are never resolved across sources at write time. Everything the
source carries beyond these stays per-source raw under `photos/<source>/`.

## Examples

```jsonl
{"ts":"2026-06-10T18:22:41-07:00","source":"apple-photos","guid":"E1B2C3D4-5F6A-7890-ABCD-1234567890AB","kind":"live","filename":"IMG_4821.HEIC","mime":"image/heic","width":4032,"height":3024,"lat":37.80331,"lon":-122.44896,"camera_make":"Apple","camera_model":"iPhone 15 Pro","favorite":true,"duration_secs":2.5,"albums":["Summer 2026","Favorites"],"people":["person-7"],"people_name":["Alice"],"extra":{"hidden":false,"labels":["beach","sunset"],"score_overall":0.84}}
{"ts":"2016-08-14T11:03:00-04:00","source":"google-photos","guid":"https://photos.google.com/photo/AF1QipMxyz123","title":"Grandma's birthday","lat":40.7128,"lon":-74.006}
{"ts":"2026-06-11T09:47:12-07:00","source":"macos-screenshots","guid":"sha256:9f2c0a7d4e","kind":"screenshot","filename":"Screenshot 2026-06-11 at 9.47.12 AM.png","mime":"image/png","width":2880,"height":1800,"text":"cargo test -p trove-core\n   Compiling trove-core v0.1.0\n    Finished test profile"}
```

## Read-time semantics (FYI for writers)

The photos reader scans `photos/*/`; creating your source folder is the
registration. Rows from different sources sort into one timeline by `ts`, and
geotagged rows join the location view at read time (a photo is a place-and-time
fix the location reader can borrow — it is never copied into `location/`). The
same underlying photo arriving from two sources stays as two rows with two
guids; cross-source dedupe (e.g. an Apple Photos asset and its Google Takeout
twin) is a read-time opinion, never a write-time merge. Write `ts` as the true
capture time — for Google Takeout that is the sidecar `photoTakenTime`, which
is authoritative over often-stripped in-file EXIF. **Never copy image bytes
into the vault**, and never persist a derived view back.
