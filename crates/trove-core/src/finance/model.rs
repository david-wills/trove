//! Canonical finance types — every backend (SimpleFIN now; file imports,
//! Amazon, other aggregators later) normalizes to these before anything
//! touches the vault, so the schema never churns when backends change.
//!
//! Amounts are signed decimal strings end-to-end, never floats.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One known account, registered in `finance/accounts.jsonl` (one line each).
///
/// The id is a Trove-assigned slug; `aliases` maps each backend's own id for
/// the account (SimpleFIN account id, Copilot account name, statement
/// fingerprint, …) so multiple backends land in the same vault account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    /// Account display name ("Freedom Unlimited").
    pub name: String,
    /// Institution display name ("Chase"). May be empty for imports.
    #[serde(default)]
    pub org: String,
    #[serde(default)]
    pub currency: String,
    /// source → that source's id for this account.
    #[serde(default)]
    pub aliases: BTreeMap<String, String>,
    /// RFC3339 local time the account was first seen.
    pub created: String,
}

/// The canonical transaction record (`finance/transactions/<account>/<year>.jsonl`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct Transaction {
    /// Stable per source: the backend's own id (SimpleFIN) or a synthesized
    /// content hash (file imports, phase 2).
    pub id: String,
    /// Vault account id (the `Account.id` slug).
    pub account: String,
    /// YYYY-MM-DD local — decides which year file the record lives in.
    pub posted: String,
    /// When the purchase actually happened, if the source distinguishes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transacted: Option<String>,
    /// Signed decimal string ("-42.17"), never a float.
    pub amount: String,
    #[serde(default)]
    pub currency: String,
    pub description: String,
    /// Normalized later by the categorization pass (phase 3); sources that
    /// already provide one (SimpleFIN sometimes does) pass it through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payee: Option<String>,
    /// Null until the categorization phase (or a Copilot import) fills it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Pending rows are ephemeral: each sync replaces the account's pending
    /// set (pending→posted often changes the id).
    #[serde(default)]
    pub pending: bool,
    /// "simplefin" | "csv-import" | "copilot" | "amazon" | …
    pub source: String,
    /// Source-specific passthrough (memo, raw fields, merge audit trail).
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Daily balance snapshot (`finance/balances/<account>.jsonl`) — one line per
/// local day, last write of the day wins. Net-worth-over-time falls out.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceSnapshot {
    /// YYYY-MM-DD local snapshot day (the upsert key).
    pub date: String,
    /// RFC3339 local time the reading was taken.
    pub ts: String,
    /// Signed decimal string.
    pub balance: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<String>,
    #[serde(default)]
    pub currency: String,
    /// When the source says the balance was computed (RFC3339), if reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<String>,
}

/// Trove-assigned account slug from institution + account names:
/// `("Chase", "Freedom Unlimited")` → `chase-freedom-unlimited`. Collisions
/// are resolved by the caller (see [`unique_slug`]).
pub fn account_slug(org: &str, name: &str) -> String {
    let raw = format!("{org} {name}");
    let mut slug = String::with_capacity(raw.len());
    let mut dash = true; // suppress leading dashes
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash {
            slug.push('-');
            dash = true;
        }
        if slug.len() >= 60 {
            break;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        slug.push_str("account");
    }
    slug
}

/// `slug`, made unique against already-registered ids by suffixing -2, -3, …
pub fn unique_slug(slug: &str, taken: &[&str]) -> String {
    if !taken.contains(&slug) {
        return slug.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{slug}-{n}");
        if !taken.contains(&candidate.as_str()) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_boring() {
        assert_eq!(account_slug("Chase", "Freedom Unlimited"), "chase-freedom-unlimited");
        assert_eq!(account_slug("", "Apple Card (David's)"), "apple-card-david-s");
        assert_eq!(account_slug("", "***"), "account");
        let long = "x".repeat(200);
        assert!(account_slug("bank", &long).len() <= 60);
    }

    #[test]
    fn slug_collisions_get_suffixes() {
        assert_eq!(unique_slug("chase", &[]), "chase");
        assert_eq!(unique_slug("chase", &["chase"]), "chase-2");
        assert_eq!(unique_slug("chase", &["chase", "chase-2"]), "chase-3");
    }
}
