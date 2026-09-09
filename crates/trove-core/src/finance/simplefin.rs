//! SimpleFIN Bridge client — the v1 aggregator backend.
//!
//! Chosen because it's the only aggregator where the relationship is
//! user ↔ service: each user signs up at bridge.simplefin.org, pays their
//! own fee, connects banks through the hosted widget, and hands Trove a
//! one-time **setup token**. The whole protocol is two requests:
//!
//! 1. Claim: the setup token base64-decodes to a claim URL; one POST there
//!    returns a long-lived **access URL** (with embedded basic-auth
//!    credentials). The setup token is burned by the claim — it is never
//!    stored. The access URL goes to the macOS Keychain ([`super::keychain`]).
//! 2. Sync: `GET {access_url}/accounts?start-date=…&pending=1` returns every
//!    connected account with balances and transactions.
//!
//! Hand-rolled over ureq: the protocol is deliberately tiny, no SDK needed.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine;
use chrono::{DateTime, Local, TimeZone};
use serde::Deserialize;

use super::keychain;
use super::model::Transaction;
use super::FinanceSyncStats;
use crate::registry::{ConnectMethod, ConnectStatus, ConnectedAccount, ConnectionDef};
use crate::vault::Vault;

/// Wire format of `GET /accounts` (SimpleFIN protocol v1.0).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SfinAccountSet {
    /// Human-readable connection warnings ("Chase may need attention").
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub accounts: Vec<SfinAccount>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SfinOrg {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
}

impl SfinOrg {
    pub fn display_name(&self) -> &str {
        self.name
            .as_deref()
            .or(self.domain.as_deref())
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SfinAccount {
    #[serde(default)]
    pub org: SfinOrg,
    pub id: String,
    pub name: String,
    /// ISO code, or a currency-definition URL for custom currencies.
    #[serde(default)]
    pub currency: String,
    /// Decimal string, kept as-is (never parsed to a float).
    pub balance: String,
    #[serde(rename = "available-balance", default)]
    pub available_balance: Option<String>,
    /// Unix epoch the balance was computed at.
    #[serde(rename = "balance-date", default)]
    pub balance_date: Option<i64>,
    #[serde(default)]
    pub transactions: Vec<SfinTransaction>,
}

impl SfinAccount {
    /// Today's balance snapshot from this fetch.
    pub fn balance_snapshot(&self, now: &DateTime<Local>) -> super::BalanceSnapshot {
        super::BalanceSnapshot {
            date: now.format("%Y-%m-%d").to_string(),
            ts: now.to_rfc3339(),
            balance: self.balance.clone(),
            available: self.available_balance.clone(),
            currency: self.currency.clone(),
            as_of: self
                .balance_date
                .and_then(|e| Local.timestamp_opt(e, 0).single())
                .map(|t| t.to_rfc3339()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SfinTransaction {
    pub id: String,
    /// Unix epoch; 0 when the institution hasn't posted it yet.
    #[serde(default)]
    pub posted: i64,
    pub amount: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub transacted_at: Option<i64>,
    #[serde(default)]
    pub pending: Option<bool>,
    #[serde(default)]
    pub payee: Option<String>,
    #[serde(default)]
    pub memo: Option<String>,
    /// Anything else a bridge sends, kept verbatim.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// One canonical record from one wire record. The transaction currency is
/// the account's (SimpleFIN transactions don't carry their own).
pub fn normalize(vault_account_id: &str, currency: &str, t: &SfinTransaction) -> Transaction {
    let mut extra = t.extra.clone();
    if let Some(memo) = &t.memo {
        extra.insert("memo".into(), serde_json::Value::String(memo.clone()));
    }
    // Pending rows may have posted=0 (not posted yet) — fall back to the
    // purchase time so they still land in a sensible day/year file.
    let posted_epoch = if t.posted != 0 {
        t.posted
    } else {
        t.transacted_at.unwrap_or(0)
    };
    Transaction {
        id: t.id.clone(),
        account: vault_account_id.to_string(),
        posted: epoch_date(posted_epoch),
        transacted: t.transacted_at.map(epoch_date),
        amount: t.amount.clone(),
        currency: currency.to_string(),
        description: t.description.clone(),
        payee: t.payee.clone(),
        category: None,
        pending: t.pending.unwrap_or(false),
        source: "simplefin".into(),
        extra,
    }
}

fn epoch_date(epoch: i64) -> String {
    Local
        .timestamp_opt(epoch, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".into())
}

/// Decode a setup token to its claim URL (no network).
pub fn decode_setup_token(token: &str) -> Result<String> {
    let cleaned: String = token.split_whitespace().collect();
    let bytes = STANDARD
        .decode(cleaned.as_bytes())
        .or_else(|_| STANDARD_NO_PAD.decode(cleaned.as_bytes()))
        .or_else(|_| URL_SAFE_NO_PAD.decode(cleaned.as_bytes()))
        .context("that doesn't look like a SimpleFIN setup token (not base64)")?;
    let url = String::from_utf8(bytes)
        .ok()
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .context("that doesn't look like a SimpleFIN setup token (no claim URL inside)")?;
    Ok(url)
}

/// Claim a setup token: one POST to the decoded claim URL returns the
/// long-lived access URL. Claiming burns the token — a second claim fails at
/// the Bridge, so the result must be stored immediately.
pub fn claim_setup_token(token: &str) -> Result<String> {
    let claim_url = decode_setup_token(token)?;
    let access_url = ureq::post(&claim_url)
        .send_string("")
        .map_err(claim_error)?
        .into_string()
        .context("reading claim response")?
        .trim()
        .to_string();
    if !access_url.starts_with("https://") && !access_url.starts_with("http://") {
        bail!("the Bridge returned an unexpected claim response");
    }
    Ok(access_url)
}

fn claim_error(e: ureq::Error) -> anyhow::Error {
    match e {
        ureq::Error::Status(403, _) => anyhow::anyhow!(
            "the Bridge refused this setup token (403) — tokens are one-time; generate a fresh one at bridge.simplefin.org"
        ),
        ureq::Error::Status(code, _) => anyhow::anyhow!("claiming the setup token failed (HTTP {code})"),
        other => anyhow::Error::new(other).context("claiming the setup token (network)"),
    }
}

/// `GET {access_url}/accounts` — the entire sync protocol. `start_epoch`
/// bounds the transaction window; pending transactions are requested too.
pub fn fetch_accounts(access_url: &str, start_epoch: Option<i64>) -> Result<SfinAccountSet> {
    let (base, auth) = split_access_url(access_url)?;
    let mut url = format!("{}/accounts?pending=1", base.trim_end_matches('/'));
    if let Some(start) = start_epoch {
        url.push_str(&format!("&start-date={start}"));
    }
    let mut req = ureq::get(&url);
    if let Some((user, pass)) = auth {
        req = req.set(
            "Authorization",
            &format!("Basic {}", STANDARD.encode(format!("{user}:{pass}"))),
        );
    }
    let resp = req.call().map_err(|e| match e {
        ureq::Error::Status(403, _) | ureq::Error::Status(401, _) => anyhow::anyhow!(
            "SimpleFIN rejected the credential — disconnect, then reconnect with a fresh setup token"
        ),
        ureq::Error::Status(code, _) => anyhow::anyhow!("SimpleFIN fetch failed (HTTP {code})"),
        other => anyhow::Error::new(other).context("SimpleFIN fetch (network)"),
    })?;
    resp.into_json().context("parsing SimpleFIN response")
}

/// Split the embedded `user:pass@` out of an access URL: ureq won't send
/// URL userinfo as basic auth itself. Credentials may be percent-encoded.
fn split_access_url(url: &str) -> Result<(String, Option<(String, String)>)> {
    let (scheme, rest) = url
        .split_once("://")
        .context("the stored access URL is malformed")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    match authority.rsplit_once('@') {
        Some((userinfo, host)) => {
            let (user, pass) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            Ok((
                format!("{scheme}://{host}{path}"),
                Some((pct_decode(user), pct_decode(pass))),
            ))
        }
        None => Ok((url.to_string(), None)),
    }
}

fn pct_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Connect: claim the token, store the access URL in the Keychain, run the
/// first sync (which requests the full history window).
pub fn connect(vault: &Vault, setup_token: &str) -> Result<FinanceSyncStats> {
    let access_url = claim_setup_token(setup_token)?;
    keychain::store_access_url(&access_url)?;
    vault.finance_sync()
}

/// Disconnect: forget the credential. Synced data stays in the vault.
pub fn disconnect(vault: &Vault) -> Result<()> {
    keychain::delete_access_url()?;
    // Clear any standing error so the hub card doesn't nag post-disconnect.
    if let Some(mut state) = vault.read_finance_sync() {
        state.error = None;
        vault.write_finance_sync(&state)?;
    }
    Ok(())
}

/// Registered in [`crate::integrations::CONNECTIONS`]; referenced by
/// [`super::BANK_SYNC_DEF`]. One method: the user pastes a one-time setup
/// token — there's no OAuth and no app-credentials step.
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "simplefin",
    display_name: "SimpleFIN",
    methods: &[ConnectMethod::TokenPaste {
        label: "Setup token",
        help: "One-time setup token from your SimpleFIN Bridge dashboard (bridge.simplefin.org → New connection/app). It's claimed once and burned; the resulting credential lives in the macOS Keychain, never in the vault.",
        placeholder: "paste the setup token from bridge.simplefin.org",
        run: connect_token,
    }],
    status: connect_status,
    disconnect: connect_disconnect,
    // connect() runs the first sync inside itself — no follow-up pull.
    auto_pull: &[],
    setup: &[
        "Create a SimpleFIN Bridge account at bridge.simplefin.org and link your banks there.",
        "Bridge dashboard → New connection/app → copy the one-time setup token and paste it here.",
    ],
};

/// [`ConnectMethod::TokenPaste`] adapter over [`connect`]: the first-sync
/// stats are the sync state file's business, not the connect phase's.
fn connect_token(vault: &Vault, token: &str) -> Result<()> {
    connect(vault, token).map(|_| ())
}

/// Cheap status probe: connected ⇔ an access URL sits in the Keychain.
/// `configured` is always true — token paste has no app-credentials concept.
fn connect_status(vault: &Vault) -> Result<ConnectStatus> {
    let Some(access_url) = keychain::load_access_url()? else {
        return Ok(ConnectStatus { configured: true, accounts: Vec::new() });
    };
    let mut extra = BTreeMap::new();
    if let Some(state) = vault.read_finance_sync() {
        if !state.updated.is_empty() {
            extra.insert("last_sync", state.updated);
        }
        if let Some(error) = state.error {
            extra.insert("error", error);
        }
    }
    Ok(ConnectStatus {
        configured: true,
        accounts: vec![ConnectedAccount {
            key: "simplefin".into(),
            label: bridge_host(&access_url).unwrap_or_else(|| "SimpleFIN".into()),
            // The claim time isn't recorded anywhere cheap.
            connected_at: None,
            // Access URLs are long-lived with no expiry we can read off the
            // credential; auth failures land as the standing sync error
            // (surfaced in `extra`), so the flag stays honest at false.
            expires_at: None,
            needs_reconnect: false,
            extra,
        }],
    })
}

/// [`ConnectionDef::disconnect`] adapter: a single credential, so the key
/// is always "simplefin". Synced data stays in the vault.
fn connect_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    disconnect(vault)
}

/// The bridge's hostname out of the stored access URL — the closest thing
/// to an org name available without a network call.
fn bridge_host(access_url: &str) -> Option<String> {
    let (base, _) = split_access_url(access_url).ok()?;
    let host = base.split_once("://")?.1.split('/').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// A realistic `/accounts` payload, shared by the tests here and in `super`.
#[cfg(test)]
pub(crate) const SAMPLE_RESPONSE: &str = r#"{
  "errors": [],
  "accounts": [
    {
      "org": {"domain": "testbank.example", "name": "Test Bank", "sfin-url": "https://sfin.testbank.example"},
      "id": "ACT-2a932bce",
      "name": "Checking",
      "currency": "USD",
      "balance": "210.13",
      "available-balance": "210.13",
      "balance-date": 1765411200,
      "transactions": [
        {"id": "TRN-1", "posted": 1765324800, "amount": "-42.17", "description": "AMZN Mktp US*123", "memo": "card 1234"},
        {"id": "TRN-2", "posted": 0, "transacted_at": 1765411200, "amount": "-9.99", "description": "COFFEE SHOP", "pending": true}
      ]
    }
  ]
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_token_decodes_to_claim_url() {
        let claim = "https://bridge.simplefin.org/simplefin/claim/demo";
        let token = STANDARD.encode(claim);
        assert_eq!(decode_setup_token(&token).unwrap(), claim);
        // Pasted with stray whitespace/newlines — still fine.
        let sloppy = format!("  {}\n", STANDARD.encode(claim));
        assert_eq!(decode_setup_token(&sloppy).unwrap(), claim);
        assert!(decode_setup_token("not base64!!!").is_err());
        // Valid base64 that isn't a URL is rejected too.
        assert!(decode_setup_token(&STANDARD.encode("hello world")).is_err());
    }

    #[test]
    fn access_url_auth_is_split_out() {
        let (base, auth) =
            split_access_url("https://u123:p%40ss@bridge.simplefin.org/simplefin").unwrap();
        assert_eq!(base, "https://bridge.simplefin.org/simplefin");
        assert_eq!(auth, Some(("u123".into(), "p@ss".into())));

        let (base, auth) = split_access_url("https://bridge.simplefin.org/simplefin").unwrap();
        assert_eq!(base, "https://bridge.simplefin.org/simplefin");
        assert_eq!(auth, None);
        assert!(split_access_url("garbage").is_err());
    }

    // connect_status / connect_token aren't unit-tested: the Keychain is
    // machine-global (a dev box may hold a real credential), so any test
    // would be flaky or touch real state. The pure pieces are covered.
    #[test]
    fn connection_def_is_token_paste_only() {
        assert_eq!(CONNECTION.id, "simplefin");
        assert!(CONNECTION.method("token-paste").is_some());
        assert!(CONNECTION.method("oauth").is_none());
        assert!(CONNECTION.auto_pull.is_empty(), "connect() runs the first sync itself");
    }

    #[test]
    fn bridge_host_comes_from_the_access_url() {
        assert_eq!(
            bridge_host("https://u123:p%40ss@bridge.simplefin.org/simplefin").as_deref(),
            Some("bridge.simplefin.org")
        );
        assert_eq!(
            bridge_host("https://beta-bridge.simplefin.org/simplefin").as_deref(),
            Some("beta-bridge.simplefin.org")
        );
        assert_eq!(bridge_host("garbage"), None);
    }

    #[test]
    fn normalize_keeps_amounts_as_strings() {
        let set: SfinAccountSet = serde_json::from_str(SAMPLE_RESPONSE).unwrap();
        let acct = &set.accounts[0];
        let t = normalize("test-bank-checking", &acct.currency, &acct.transactions[0]);
        assert_eq!(t.amount, "-42.17");
        assert_eq!(t.currency, "USD");
        assert!(!t.pending);
        assert_eq!(t.source, "simplefin");
        assert_eq!(t.extra.get("memo").and_then(|v| v.as_str()), Some("card 1234"));
        // Posted epoch → a real local date, not a unix number.
        assert_eq!(t.posted.len(), 10);
        assert!(t.posted.starts_with("20"), "got {}", t.posted);
    }

    #[test]
    fn pending_with_unposted_date_uses_transacted_at() {
        let set: SfinAccountSet = serde_json::from_str(SAMPLE_RESPONSE).unwrap();
        let acct = &set.accounts[0];
        let t = normalize("a", &acct.currency, &acct.transactions[1]);
        assert!(t.pending);
        assert!(t.posted.starts_with("20"), "fell back to transacted_at, got {}", t.posted);
        assert_eq!(t.transacted.as_deref(), Some(t.posted.as_str()));
    }
}
