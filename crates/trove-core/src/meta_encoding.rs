//! Meta "mojibake" repair, shared by the Meta Download-Your-Information
//! importers (`facebook`, and `facebook-messenger` #72 once built).
//!
//! Meta's DYI JSON stores text as UTF-8 *bytes* that were then mis-decoded as
//! Latin-1 (ISO-8859-1) and re-encoded as JSON. The classic symptom: an emoji
//! or accented letter shows up as a run of garbled Latin-1 characters
//! (`"ð"` for a `😀`). Because the original bytes were
//! valid UTF-8, the repair is exact and reversible: read each `char` back as
//! the single byte it must have been, then re-interpret the byte sequence as
//! UTF-8.
//!
//! [`fix_meta_encoding`] is **conservative** — it only rewrites a string when
//! every `char` is in the Latin-1 range (`<= U+00FF`, i.e. a single byte) AND
//! the reconstructed byte sequence is itself valid UTF-8. Anything else — a
//! string already carrying a real (multi-byte) emoji, plain ASCII, or a byte
//! sequence that doesn't round-trip — is returned **unchanged**. Pure ASCII is
//! a no-op (every byte re-decodes to itself); already-correct emoji are
//! preserved (they contain chars `> U+00FF`, so the guard short-circuits).
//!
//! Apply it to *every* string value parsed out of a DYI export (post text,
//! titles, tag names, attachment descriptions, friend names, search terms, …),
//! not just one field — the mojibake is pervasive across the whole export.

use serde_json::Value;

/// Repair a single Meta-DYI mojibake string, or return it unchanged when it
/// doesn't look mis-encoded. See the module docs for the exact rule.
pub(crate) fn fix_meta_encoding(s: &str) -> String {
    // Guard: every char must be a single Latin-1 byte. A char above U+00FF
    // means the string already holds real multi-byte text (a correct emoji,
    // CJK, …) — leave it alone.
    if s.chars().any(|c| c as u32 > 0xFF) {
        return s.to_string();
    }
    // Each char is the byte it was mis-decoded from.
    let bytes: Vec<u8> = s.chars().map(|c| c as u8).collect();
    // Re-interpret those bytes as UTF-8. Only adopt the result when it is
    // valid UTF-8 (the original always was); otherwise the string wasn't
    // mojibake we can safely repair — keep it verbatim.
    match String::from_utf8(bytes) {
        Ok(fixed) => fixed,
        Err(_) => s.to_string(),
    }
}

/// Recursively repair every string value inside a parsed JSON `Value`
/// in place: object values, array elements, and bare strings. Object *keys*
/// are left as-is — DYI keys are stable ASCII field names, only the values
/// carry user text.
pub(crate) fn fix_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            let fixed = fix_meta_encoding(s);
            if fixed != *s {
                *s = fixed;
            }
        }
        Value::Array(items) => {
            for item in items {
                fix_value(item);
            }
        }
        Value::Object(map) => {
            for (_k, v) in map.iter_mut() {
                fix_value(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn repairs_mojibake_emoji() {
        // "😀" (U+1F600) is UTF-8 bytes F0 9F 98 80; mis-decoded as Latin-1
        // those bytes become the four chars below. The repair must recover it.
        let mojibake = "\u{00f0}\u{009f}\u{0098}\u{0080}";
        assert_eq!(fix_meta_encoding(mojibake), "😀");
    }

    #[test]
    fn repairs_mojibake_accented_text() {
        // "café" → "caf" + UTF-8 of é (C3 A9) mis-read as Latin-1 (Ã©).
        let mojibake = "caf\u{00c3}\u{00a9}";
        assert_eq!(fix_meta_encoding(mojibake), "café");
    }

    #[test]
    fn pure_ascii_passes_through() {
        assert_eq!(fix_meta_encoding("Hello, world!"), "Hello, world!");
        assert_eq!(fix_meta_encoding(""), "");
    }

    #[test]
    fn already_correct_emoji_preserved() {
        // A string that already contains a real multi-byte emoji has chars
        // above U+00FF → the guard short-circuits and returns it verbatim.
        assert_eq!(fix_meta_encoding("nice 😀 day"), "nice 😀 day");
        assert_eq!(fix_meta_encoding("日本語"), "日本語");
    }

    #[test]
    fn invalid_utf8_byte_sequence_left_unchanged() {
        // A lone Latin-1 char that is NOT part of a valid UTF-8 sequence (a
        // stray 0xE9 byte with no continuation) cannot be safely repaired —
        // return it unchanged rather than corrupting it.
        let stray = "\u{00e9}";
        assert_eq!(fix_meta_encoding(stray), stray);
    }

    #[test]
    fn fix_value_is_recursive() {
        let mut v = json!({
            "text": "caf\u{00c3}\u{00a9}",
            "tags": ["\u{00f0}\u{009f}\u{0098}\u{0080}", "plain"],
            "nested": {"name": "caf\u{00c3}\u{00a9}", "n": 5},
            "kept": "ascii"
        });
        fix_value(&mut v);
        assert_eq!(v["text"], json!("café"));
        assert_eq!(v["tags"][0], json!("😀"));
        assert_eq!(v["tags"][1], json!("plain"));
        assert_eq!(v["nested"]["name"], json!("café"));
        assert_eq!(v["nested"]["n"], json!(5));
        assert_eq!(v["kept"], json!("ascii"));
    }
}
