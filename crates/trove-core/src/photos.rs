//! The `photos` domain contract: photo and video **metadata** — one record per
//! asset, from every photo source: the Mac's Apple Photos library, loose files
//! via EXIF import, a Google Photos Takeout, Flickr / SmugMug archives, a BeReal
//! export, and the screenshot watcher. **Metadata only, never image bytes** —
//! the contract indexes when, where, and on what a picture was taken; the pixels
//! stay wherever the user keeps them, and full source fidelity lives in each
//! source's own raw folder.
//!
//! Each source writes rows under `photos/<source>/YYYY-MM.jsonl` (`<source>` is
//! the collector id and folder name; the month is the month of [`Photo::ts`]).
//! The stream is **append-only** — a photo is taken once — and [`Photo::guid`]
//! is the dedupe key (source-unique: an Apple Photos asset UUID, an EXIF file's
//! content hash, a Google sidecar URL, a Flickr/SmugMug image id, a BeReal post
//! id, a screenshot's content hash). Re-imports of overlapping exports must skip
//! already-stored guids before appending. The same underlying photo arriving
//! from two sources stays as two rows with two guids; cross-source dedupe is a
//! read-time opinion, never a write-time merge.
//!
//! GPS geotags make this a **privacy-sensitive** location proxy: every source
//! ships opt-in with explicit acknowledgement. Faces / people are also
//! privacy-sensitive and opt-in — off by default ([`Photo::people`] /
//! [`Photo::people_name`] are written only when the user enables them, as raw
//! source handles/labels never resolved across sources at write time). First
//! collector: `exif-import` (loose files dragged in), which carries no face
//! data and never emits the people columns.
//!
//! See [`docs/vault-spec/domains/photos.md`] for the field-level spec.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One asset — one line of `photos/<source>/YYYY-MM.jsonl`.
///
/// An *event* record (it has a `ts`), not a snapshot. Only `ts`/`source`/`guid`
/// are required — a GPS-stripped scan or a bare Takeout sidecar writes just
/// those plus whatever it has, while a rich library asset fills dimensions, GPS,
/// camera, albums, and flags. Everything a source exposes that has no column
/// here is preserved verbatim under [`extra`](Photo::extra) (altitude, lens,
/// orientation, file size, ML scene labels, …) rather than dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Photo {
    /// RFC3339 local capture time (photo taken / video recorded / screenshot
    /// saved). Always serialized; its month is the partition key.
    pub ts: String,
    /// Collector id, identical to the source folder name (`apple-photos`,
    /// `exif-import`, `google-photos`). Always serialized.
    pub source: String,
    /// Source-unique id, the dedupe key (Apple Photos asset UUID, an EXIF
    /// file's content hash, a Google sidecar URL, a Flickr/SmugMug image id, a
    /// BeReal post id, a screenshot's content hash). Always serialized.
    pub guid: String,
    /// `"photo"` (default) | `"video"` | `"live"` | `"screenshot"` | `"burst"`
    /// — an open string; readers stay lenient to values beyond the documented
    /// set.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    /// Original file name (no path).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub filename: String,
    /// Caption / title where the source has one (Google description,
    /// Flickr/SmugMug caption).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// MIME type (`"image/heic"`, `"image/jpeg"`, `"video/quicktime"`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mime: String,
    /// Pixel width.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    /// Pixel height.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// Latitude, decimal degrees (WGS84). Omitted when the asset carries no
    /// geotag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat: Option<f64>,
    /// Longitude, decimal degrees (WGS84). Omitted when the asset carries no
    /// geotag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon: Option<f64>,
    /// EXIF camera/device make (`"Apple"`, `"Canon"`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub camera_make: String,
    /// EXIF camera/device model (`"iPhone 15 Pro"`, …).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub camera_model: String,
    /// Flagged a favorite / faved on the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorite: Option<bool>,
    /// Clip length, in seconds, for `"video"` / `"live"` assets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<f64>,
    /// Album / set names this asset belongs to (verbatim).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub albums: Vec<String>,
    /// Source-native tags / keywords (Flickr tags, SmugMug keywords, EXIF
    /// keywords).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Face/person cluster ids or labels (**opt-in**; raw source handles, not
    /// resolved across sources). Off by default — written only when the user
    /// enables faces; `exif-import` has no face data and never emits this.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub people: Vec<String>,
    /// Display names, positionally paired with [`people`](Photo::people) where
    /// the source gives both. Same opt-in gating as `people`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub people_name: Vec<String>,
    /// Extracted on-device OCR text (screenshots; never sent off-device).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Everything source-specific the normalized fields don't carry — altitude,
    /// lens, orientation, file size, ML scene labels, view/fave counts, BeReal
    /// front/back file names, hidden flag — full fidelity.
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Photo {
    /// A minimal record with only the three required fields set.
    pub fn new(source: impl Into<String>, guid: impl Into<String>, ts: impl Into<String>) -> Self {
        Photo {
            ts: ts.into(),
            source: source.into(),
            guid: guid.into(),
            kind: String::new(),
            filename: String::new(),
            title: String::new(),
            mime: String::new(),
            width: None,
            height: None,
            lat: None,
            lon: None,
            camera_make: String::new(),
            camera_model: String::new(),
            favorite: None,
            duration_secs: None,
            albums: Vec::new(),
            tags: Vec::new(),
            people: Vec::new(),
            people_name: Vec::new(),
            text: String::new(),
            extra: Map::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_record_serializes_only_required_fields() {
        let p = Photo::new("exif-import", "sha256:9f2c0a7d4e", "2026-06-02T11:05:00-07:00");
        // Omit-empty: a sparse line is exactly the three required keys.
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            json!({"ts": "2026-06-02T11:05:00-07:00", "source": "exif-import", "guid": "sha256:9f2c0a7d4e"})
        );
    }

    #[test]
    fn full_record_round_trips() {
        let line = json!({
            "ts": "2026-06-10T18:22:41-07:00",
            "source": "apple-photos",
            "guid": "E1B2C3D4-5F6A-7890-ABCD-1234567890AB",
            "kind": "live",
            "filename": "IMG_4821.HEIC",
            "mime": "image/heic",
            "width": 4032,
            "height": 3024,
            "lat": 37.80331,
            "lon": -122.44896,
            "camera_make": "Apple",
            "camera_model": "iPhone 15 Pro",
            "favorite": true,
            "duration_secs": 2.5,
            "albums": ["Summer 2026", "Favorites"],
            "people": ["person-7"],
            "people_name": ["Alice"],
            "extra": {"hidden": false, "labels": ["beach", "sunset"], "score_overall": 0.84}
        });
        let p: Photo = serde_json::from_value(line.clone()).unwrap();
        assert_eq!(p.kind, "live");
        assert_eq!(p.width, Some(4032));
        assert_eq!(p.lat, Some(37.80331));
        assert_eq!(p.favorite, Some(true));
        assert_eq!(p.duration_secs, Some(2.5));
        assert_eq!(p.albums.len(), 2);
        assert_eq!(p.people_name, vec!["Alice"]);
        assert_eq!(serde_json::to_value(&p).unwrap(), line);
    }

    #[test]
    fn unknown_fields_tolerated_and_extra_round_trips() {
        // Forward-compat: an unknown top-level field is ignored; `extra` carries
        // the source-specific bag verbatim (lens / orientation / altitude live
        // here, never as columns).
        let line = json!({
            "ts": "2016-08-14T11:03:00-04:00",
            "source": "google-photos",
            "guid": "https://photos.google.com/photo/AF1QipMxyz123",
            "title": "Grandma's birthday",
            "future_field": "ignored",
            "extra": {"lens_model": "iPhone 15 Pro back camera 6.86mm f/1.78", "orientation": 6, "altitude": 12.3}
        });
        let p: Photo = serde_json::from_value(line).unwrap();
        assert_eq!(p.title, "Grandma's birthday");
        assert_eq!(p.extra.get("orientation"), Some(&json!(6)));
        // Re-serialized form drops the unknown field but keeps extra.
        let re = serde_json::to_value(&p).unwrap();
        assert!(re.get("future_field").is_none());
        assert_eq!(re["extra"]["lens_model"], json!("iPhone 15 Pro back camera 6.86mm f/1.78"));
    }
}
