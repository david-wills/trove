//! Emporia Vue home energy monitor — per-circuit kWh via the unofficial
//! Emporia cloud API.  Catalogued in the Phase 2 pass; brief:
//! docs/integrations/emporia.md
//!
//! ## Auth
//!
//! The Emporia app uses **AWS Cognito** (pool `us-east-2_ghlOXVLi1`, client
//! `4qte47jbstod8apnfic0bunmrq`).  Cognito uses the SRP (Secure Remote
//! Password) protocol: the client never sends the password in the clear —
//! instead it computes a proof using BigInteger arithmetic over a 3072-bit
//! prime group and a server-supplied challenge (`SRP_B`, `SALT`,
//! `SECRET_BLOCK`).  We implement the SRP math in pure Rust (no `aws-sdk`
//! async runtime, no C crypto) using `num-bigint` + the `sha2`/`hmac`
//! primitives already in the dependency tree.
//!
//! Tokens (`id_token`, `refresh_token`, `expires_at`) are stored 0600 in the
//! vault's `.trove/sync/` secret store.  The `id_token` is passed as the
//! `authtoken` header on all data requests; the `refresh_token` is exchanged
//! for a fresh `id_token` when it's about to expire.
//!
//! ## Data
//!
//! - `GET /customers/devices` → device list (device GIDs + channel names).
//! - `GET /AppAPI?apiMethod=getChartUsage&...` → per-circuit kWh list for a
//!   time window at a chosen resolution (`1MIN` / `1H` / `1D`).
//!
//! Default cadence: hourly aggregate for the backfill, minute aggregate for
//! incremental polling.  A depth toggle (future) can enable the 1-second
//! firehose.
//!
//! ## Vault layout
//!
//! - **Raw layer** (`home/emporia/raw/YYYY-MM.jsonl`) — full API responses,
//!   one object per device-channel-window per poll, tagged with `ts`.
//! - **Energy layer** (`home/emporia/energy/YYYY-MM.jsonl`) — home.energy
//!   draft-schema rows (fields: `ts`, `source`, `device`, `circuit`, `kwh`,
//!   `interval_secs`, `direction`, `guid`).  The home.energy shape is an
//!   **unbound sibling draft**; these rows follow the schema field-for-field
//!   for forward compatibility but are NOT backed by a Rust type yet.
//!
//! `guid` = `emporia:{deviceGid}:{channelNum}:{interval_start_utc_secs}:{scale}`.
//!
//! ## Cursor
//!
//! A non-secret rebuildable cursor at `.trove/emporia-sync.json` stores per-
//! channel watermarks (the latest interval `ts` written, epoch seconds) so
//! incremental polls only fetch new buckets.

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;
use chrono::{DateTime, Datelike, Local, TimeZone, Timelike, Utc};
use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::integrations::{Integration, IntegrationKind};
use crate::registry::{
    Behavior, Cadence, CollectOutcome, ConnectMethod, ConnectStatus, ConnectedAccount,
    ConnectionDef, IntegrationDef, PullOutcome,
};
use crate::store::Partition;
use crate::sync::oauth::TokenSet;
use crate::vault::Vault;

// ---------------------------------------------------------------------------
// Paths and constants.

const RAW_DIR: &str = "home/emporia/raw";
const ENERGY_DIR: &str = "home/emporia/energy";
const SYNC_FILE: &str = ".trove/emporia-sync.json";
const SERVICE: &str = "emporia";

const EMPORIA_API: &str = "https://api.emporiaenergy.com";
/// Cognito pool/client for the Emporia app (reverse-engineered via pyemvue).
const COGNITO_POOL_ID: &str = "us-east-2_ghlOXVLi1";
const COGNITO_CLIENT_ID: &str = "4qte47jbstod8apnfic0bunmrq";
const COGNITO_REGION: &str = "us-east-2";
/// How often to poll in the periodic cadence (15-minute intervals, fetching
/// minute-level data since the last watermark).
const POLL_SECS: u64 = 900;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Registry face.

fn def_last_data(vault: &Vault) -> Option<String> {
    crate::registry::newest_stem(&vault.root().join(ENERGY_DIR))
}

fn def_collect(vault: &Vault, _now: DateTime<Local>) -> Result<CollectOutcome> {
    match pull(vault) {
        Ok(n) => Ok(CollectOutcome::note_if(n > 0, || {
            format!("Emporia synced — {n} energy intervals")
        })),
        Err(e) => Ok(CollectOutcome::note(format!("Emporia sync skipped: {e}"))),
    }
}

fn def_pull(vault: &Vault) -> Result<PullOutcome> {
    let n = pull(vault)?;
    let headline = if n == 0 {
        "Emporia is up to date — no new energy intervals".to_string()
    } else {
        format!("Emporia synced — {n} energy intervals")
    };
    Ok(PullOutcome { headline, counts: BTreeMap::from([("energy_intervals", n)]) })
}

/// Registered in [`crate::integrations::INTEGRATIONS`] (the stub line is
/// already there — do NOT add another).
pub static DEF: IntegrationDef = IntegrationDef {
    meta: Integration {
        id: "emporia",
        name: "Emporia Vue",
        kind: IntegrationKind::CloudSync,
        default_on: false,
        description: "Collects per-circuit electricity usage from your Emporia Vue \
                      energy monitor. Tracks whole-home and individual-circuit kWh \
                      over time. First sync backfills available history.",
        domain: "home",
        vault_path: "home/emporia/",
        toggleable: true,
        setup: &[
            "Connect your Emporia account credentials on this card.",
            "First sync backfills your history; later syncs are incremental.",
            "Circuit names come from your Emporia app — rename them there \
             and they update on the next sync.",
        ],
        caveats: "Uses an unofficial, community-maintained API (pyemvue / vuegraf) \
                  that Emporia acknowledges but does not officially support — it may \
                  break without notice if Emporia changes their backend. \
                  There is no official API or CSV export; this is the only data path.",
    },
    behavior: Behavior::Periodic {
        cadence: Cadence::every_on_run(POLL_SECS),
        collect: def_collect,
    },
    permission: None,
    last_data: Some(def_last_data),
    connection: Some("emporia"),
    pull: Some(def_pull),
};

// ---------------------------------------------------------------------------
// Connection definition.

fn def_connect(vault: &Vault, pasted: &str) -> Result<()> {
    let (username, password) = parse_credentials(pasted)?;
    // Authenticate via Cognito SRP to verify credentials and store tokens.
    let tokens = cognito_srp_login(&username, &password)
        .context("Emporia login failed — check your email and password")?;
    vault.save_sync_token(SERVICE, &tokens)
}

fn def_disconnect(vault: &Vault, _key: &str) -> Result<()> {
    vault.delete_sync_token(SERVICE)
}

fn def_status(vault: &Vault) -> Result<ConnectStatus> {
    let mut accounts = Vec::new();
    if let Some(tok) = vault.load_sync_token(SERVICE)? {
        // The username is stored in token_type (convention for non-OAuth services).
        let label = tok.token_type.unwrap_or_else(|| "Emporia Account".to_string());
        accounts.push(ConnectedAccount {
            key: SERVICE.to_string(),
            label,
            connected_at: None,
            expires_at: tok.expires_at,
            needs_reconnect: false,
            extra: BTreeMap::new(),
        });
    }
    Ok(ConnectStatus { configured: true, accounts })
}

/// Registered in [`crate::integrations::CONNECTIONS`] (the integrator adds
/// the one `&crate::emporia::CONNECTION,` line — do NOT add it here).
pub static CONNECTION: ConnectionDef = ConnectionDef {
    id: "emporia",
    display_name: "Emporia Vue",
    methods: &[ConnectMethod::TokenPaste {
        label: "Emporia account email and password",
        help: "Enter your Emporia account email and password as email:password. \
               These credentials authenticate against Emporia's cloud service and \
               are stored locally (0600) — never sent anywhere except Emporia's \
               official servers. This integration uses an unofficial, \
               community-maintained API path.",
        placeholder: "you@example.com:yourpassword",
        run: def_connect,
    }],
    status: def_status,
    disconnect: def_disconnect,
    auto_pull: &["emporia"],
    setup: &[
        "Use your existing Emporia app account email and password.",
        "Paste them as email:password on this card.",
        "Note: this integration uses an unofficial API — it may break if \
         Emporia changes their backend.",
    ],
};

// ---------------------------------------------------------------------------
// Credential parsing.

fn parse_credentials(pasted: &str) -> Result<(String, String)> {
    let s = pasted.trim();
    if s.is_empty() {
        bail!("empty — paste your Emporia credentials as email:password");
    }
    // Split on the FIRST `:` — passwords may contain `:`.
    match s.split_once(':') {
        Some((u, p)) => {
            let u = u.trim().to_string();
            let p = p.trim().to_string();
            if u.is_empty() {
                bail!("missing email address");
            }
            if p.is_empty() {
                bail!("missing password");
            }
            Ok((u, p))
        }
        None => bail!("paste credentials as email:password"),
    }
}

// ---------------------------------------------------------------------------
// AWS Cognito SRP authentication (pure-Rust, no async runtime).
//
// Protocol: USER_SRP_AUTH flow
//  1. InitiateAuth → get SRP_B, SALT, SECRET_BLOCK from server
//  2. Compute password proof using BigInteger SRP math
//  3. RespondToAuthChallenge → get tokens
//
// Math follows the pyemvue / pycognito implementation (RFC 2945 / AWS variant).

/// 3072-bit safe prime N from RFC 3526 group 15 (the prime Cognito uses).
const N_HEX: &str = "\
FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1\
29024E088A67CC74020BBEA63B139B22514A08798E3404DD\
EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245\
E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED\
EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3D\
C2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F\
83655D23DCA3AD961C62F356208552BB9ED529077096966D\
670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B\
E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9\
DE2BCBF6955817183995497CEA956AE515D2261898FA0510\
15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64\
ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7\
ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6B\
F12FFA06D98A0864D87602733EC86A64521F2B18177B200C\
BBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3\
143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF";

const G_HEX: &str = "2";

/// Pad a BigUint to its hex representation padded to an even byte boundary,
/// with a leading zero byte if the top bit is set (positive sign for SRP).
fn pad_hex(n: &BigUint) -> String {
    let hex = format!("{:X}", n);
    // Pad to even length.
    let hex = if hex.len() % 2 == 1 { format!("0{hex}") } else { hex };
    // Prepend zero byte if the high bit would be set (big-endian positive).
    if u8::from_str_radix(&hex[..2], 16).unwrap_or(0) >= 0x80 {
        format!("00{hex}")
    } else {
        hex
    }
}

/// SHA-256 of raw bytes, returned as hex string.
fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:X}", h.finalize())
}

/// Compute `k = SHA256(PAD(N) || PAD(g))` used in SRP.
fn compute_k(n: &BigUint, g: &BigUint) -> BigUint {
    let n_hex = pad_hex(n);
    let g_hex = pad_hex(g);
    let mut combined = Vec::new();
    combined.extend_from_slice(&hex::decode(&n_hex).unwrap());
    combined.extend_from_slice(&hex::decode(&g_hex).unwrap());
    let hash_hex = sha256_hex(&combined);
    BigUint::parse_bytes(hash_hex.as_bytes(), 16).unwrap()
}

/// Compute `u = SHA256(PAD(A) || PAD(B))`.
fn compute_u(a: &BigUint, b: &BigUint) -> BigUint {
    let a_hex = pad_hex(a);
    let b_hex = pad_hex(b);
    let mut combined = Vec::new();
    combined.extend_from_slice(&hex::decode(&a_hex).unwrap());
    combined.extend_from_slice(&hex::decode(&b_hex).unwrap());
    let hash_hex = sha256_hex(&combined);
    BigUint::parse_bytes(hash_hex.as_bytes(), 16).unwrap()
}

/// Compute `x = SHA256(salt_bytes || SHA256(pool_name || username:password))`.
fn compute_x(pool_name: &str, username: &str, password: &str, salt_hex: &str) -> BigUint {
    let inner = {
        let mut h = Sha256::new();
        h.update(pool_name.as_bytes());
        h.update(username.as_bytes());
        h.update(b":");
        h.update(password.as_bytes());
        h.finalize()
    };
    let salt_bytes = hex::decode(salt_hex).unwrap_or_default();
    let outer_hex = {
        let mut h = Sha256::new();
        h.update(&salt_bytes);
        h.update(&inner);
        format!("{:X}", h.finalize())
    };
    BigUint::parse_bytes(outer_hex.as_bytes(), 16).unwrap()
}

/// HKDF-SHA256 with 16 bytes of output.
/// info = b"Caldera Derived Key\x01"
fn hkdf_16(ikm: &[u8], salt: &[u8]) -> [u8; 16] {
    type HmacSha256 = Hmac<Sha256>;
    // Extract step: prk = HMAC-SHA256(salt, ikm)
    let mut mac = HmacSha256::new_from_slice(salt).expect("hmac");
    mac.update(ikm);
    let prk = mac.finalize().into_bytes();
    // Expand step: output = HMAC-SHA256(prk, info || 0x01)[:16]
    let info_bytes = b"Caldera Derived Key\x01";
    let mut mac = HmacSha256::new_from_slice(&prk).expect("hmac");
    mac.update(info_bytes);
    let expanded = mac.finalize().into_bytes();
    let mut out = [0u8; 16];
    out.copy_from_slice(&expanded[..16]);
    out
}

/// Make the AWS Cognito SRP timestamp string (e.g. "Mon Jan 15 14:30:45 UTC 2024").
fn cognito_timestamp() -> String {
    let now = Utc::now();
    // Cognito uses abbreviated English weekday/month names.
    let weekdays = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
                  "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let wd = weekdays[now.weekday().num_days_from_sunday() as usize];
    let mo = months[(now.month() - 1) as usize];
    format!("{wd} {mo} {:2} {:02}:{:02}:{:02} UTC {}",
            now.day(), now.hour(), now.minute(), now.second(), now.year())
}

/// Thin wrapper: POST to Cognito's regional identity endpoint.
fn cognito_post(body: &Value) -> Result<Value> {
    let url = format!(
        "https://cognito-idp.{COGNITO_REGION}.amazonaws.com/{COGNITO_POOL_ID}"
    );
    let resp = ureq::post(&url)
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/x-amz-json-1.1")
        .set("X-Amz-Target", "AWSCognitoIdentityProviderService.InitiateAuth")
        .send_json(body);
    match resp {
        Ok(r) => Ok(r.into_json::<Value>().context("parsing Cognito response")?),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            bail!("Cognito returned HTTP {code}: {}", &body[..body.len().min(400)])
        }
        Err(e) => bail!("Cognito request failed: {e}"),
    }
}

/// POST the RespondToAuthChallenge step (uses a different Amz-Target).
fn cognito_respond(body: &Value) -> Result<Value> {
    let url = format!(
        "https://cognito-idp.{COGNITO_REGION}.amazonaws.com/{COGNITO_POOL_ID}"
    );
    let resp = ureq::post(&url)
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/x-amz-json-1.1")
        .set("X-Amz-Target", "AWSCognitoIdentityProviderService.RespondToAuthChallenge")
        .send_json(body);
    match resp {
        Ok(r) => Ok(r.into_json::<Value>().context("parsing Cognito response")?),
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            bail!("Cognito returned HTTP {code}: {}", &body[..body.len().min(400)])
        }
        Err(e) => bail!("Cognito request failed: {e}"),
    }
}

/// Full Cognito SRP login.  Returns a `TokenSet` with `access_token` =
/// id_token (the one Emporia uses as `authtoken`), `refresh_token`, and
/// `token_type` = the username (for the status label).
fn cognito_srp_login(username: &str, password: &str) -> Result<TokenSet> {
    let n = BigUint::parse_bytes(N_HEX.replace('\n', "").as_bytes(), 16)
        .context("parse N")?;
    let g = BigUint::parse_bytes(G_HEX.as_bytes(), 16).context("parse g")?;

    // Generate random client secret `a` (128 random bytes).
    let mut a_bytes = [0u8; 128];
    getrandom::getrandom(&mut a_bytes).context("rng")?;
    let a = BigUint::from_bytes_be(&a_bytes) % &n;
    let capital_a = g.modpow(&a, &n);
    let srp_a = format!("{:X}", capital_a);

    // Step 1: InitiateAuth (USER_SRP_AUTH)
    let pool_name = COGNITO_POOL_ID.split('_').nth(1).unwrap_or(COGNITO_POOL_ID);
    let init_body = json!({
        "AuthFlow": "USER_SRP_AUTH",
        "ClientId": COGNITO_CLIENT_ID,
        "AuthParameters": {
            "USERNAME": username,
            "SRP_A": srp_a,
        }
    });
    let init_resp = cognito_post(&init_body)?;

    // Check for challenge.
    let challenge = init_resp.get("ChallengeName").and_then(Value::as_str).unwrap_or("");
    if challenge != "PASSWORD_VERIFIER" {
        bail!("unexpected Cognito challenge: {challenge}");
    }
    let params = init_resp
        .get("ChallengeParameters")
        .context("no ChallengeParameters")?;
    let srp_b_hex = params.get("SRP_B").and_then(Value::as_str).context("no SRP_B")?;
    let salt_hex = params.get("SALT").and_then(Value::as_str).context("no SALT")?;
    let secret_block_b64 =
        params.get("SECRET_BLOCK").and_then(Value::as_str).context("no SECRET_BLOCK")?;
    // Cognito returns the username it uses internally (may differ if email alias).
    let internal_user = params
        .get("USER_ID_FOR_SRP")
        .and_then(Value::as_str)
        .unwrap_or(username);

    // Parse server B value.
    let b_str = srp_b_hex.replace('\n', "").to_uppercase();
    let capital_b = BigUint::parse_bytes(b_str.as_bytes(), 16).context("parse B")?;

    // Compute SRP values.
    let k = compute_k(&n, &g);
    let u = compute_u(&capital_a, &capital_b);
    let x = compute_x(pool_name, internal_user, password, salt_hex);

    // S = (B - k * g^x)^(a + u*x) mod N
    let gx = g.modpow(&x, &n);
    let kgx = (k * &gx) % &n;
    // In BigUint arithmetic we must handle the subtraction mod N.
    let b_minus_kgx = if capital_b >= kgx {
        &capital_b - &kgx
    } else {
        &n - (&kgx - &capital_b) % &n
    };
    let exp = (&a + &u * &x) % (&n - BigUint::from(1u32));
    let s = b_minus_kgx.modpow(&exp, &n);

    // HKDF: key = HKDF-SHA256(pad(S), pad(u)) → 16 bytes.
    let s_bytes = hex::decode(pad_hex(&s)).unwrap_or_default();
    let u_bytes = hex::decode(pad_hex(&u)).unwrap_or_default();
    let hkdf_key = hkdf_16(&s_bytes, &u_bytes);

    // Password claim signature = HMAC-SHA256(key, pool_name || user || secret_block || ts)
    let secret_block_bytes = base64::engine::general_purpose::STANDARD
        .decode(secret_block_b64)
        .context("decode SECRET_BLOCK")?;
    let ts = cognito_timestamp();
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(&hkdf_key).expect("hmac key");
    mac.update(pool_name.as_bytes());
    mac.update(internal_user.as_bytes());
    mac.update(&secret_block_bytes);
    mac.update(ts.as_bytes());
    let signature = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    // Step 2: RespondToAuthChallenge
    let respond_body = json!({
        "ChallengeName": "PASSWORD_VERIFIER",
        "ClientId": COGNITO_CLIENT_ID,
        "ChallengeResponses": {
            "USERNAME": internal_user,
            "PASSWORD_CLAIM_SECRET_BLOCK": secret_block_b64,
            "TIMESTAMP": ts,
            "PASSWORD_CLAIM_SIGNATURE": signature,
        }
    });
    let resp = cognito_respond(&respond_body)?;

    // Extract tokens.
    let auth = resp.get("AuthenticationResult").context("no AuthenticationResult")?;
    let id_token = auth.get("IdToken").and_then(Value::as_str).context("no IdToken")?;
    let refresh = auth.get("RefreshToken").and_then(Value::as_str).map(str::to_string);
    let expires_in: u64 = auth
        .get("ExpiresIn")
        .and_then(Value::as_u64)
        .unwrap_or(3600);
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    Ok(TokenSet {
        access_token: id_token.to_string(),
        refresh_token: refresh,
        token_type: Some(username.to_string()), // stored as the user label
        scope: None,
        expires_at: Some(now_epoch + expires_in),
    })
}

/// Refresh the id_token using the stored refresh_token.  On failure the
/// caller should prompt a reconnect.
fn refresh_tokens(refresh_token: &str) -> Result<TokenSet> {
    let url = format!(
        "https://cognito-idp.{COGNITO_REGION}.amazonaws.com/{COGNITO_POOL_ID}"
    );
    let body = json!({
        "AuthFlow": "REFRESH_TOKEN_AUTH",
        "ClientId": COGNITO_CLIENT_ID,
        "AuthParameters": {
            "REFRESH_TOKEN": refresh_token,
        }
    });
    let resp = ureq::post(&url)
        .timeout(HTTP_TIMEOUT)
        .set("Content-Type", "application/x-amz-json-1.1")
        .set("X-Amz-Target", "AWSCognitoIdentityProviderService.InitiateAuth")
        .send_json(&body);
    let resp = match resp {
        Ok(r) => r.into_json::<Value>().context("parse refresh response")?,
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            bail!("token refresh HTTP {code}: {}", &body[..body.len().min(300)])
        }
        Err(e) => bail!("token refresh failed: {e}"),
    };
    let auth = resp.get("AuthenticationResult").context("no AuthenticationResult in refresh")?;
    let id_token = auth.get("IdToken").and_then(Value::as_str).context("no IdToken")?;
    let expires_in: u64 = auth.get("ExpiresIn").and_then(Value::as_u64).unwrap_or(3600);
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(TokenSet {
        access_token: id_token.to_string(),
        refresh_token: Some(refresh_token.to_string()),
        token_type: None,
        scope: None,
        expires_at: Some(now_epoch + expires_in),
    })
}

// ---------------------------------------------------------------------------
// Emporia REST client.

struct EmporiaClient {
    id_token: String,
}

impl EmporiaClient {
    /// GET from the Emporia API with the id_token header.
    fn get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, EmporiaError> {
        let url = format!("{EMPORIA_API}/{path}");
        let mut req = ureq::get(&url)
            .timeout(HTTP_TIMEOUT)
            .set("authtoken", &self.id_token);
        for (k, v) in query {
            req = req.query(k, v);
        }
        match req.call() {
            Ok(r) => r
                .into_json::<Value>()
                .map_err(|e| EmporiaError::Other(format!("parse: {e}"))),
            Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => {
                Err(EmporiaError::Unauthorized)
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                Err(EmporiaError::Other(format!("HTTP {code}: {}", &body[..body.len().min(300)])))
            }
            Err(e) => Err(EmporiaError::Other(e.to_string())),
        }
    }

    /// Fetch the customer's device list.
    fn devices(&self) -> Result<Value, EmporiaError> {
        self.get("customers/devices", &[])
    }

    /// Fetch chart usage for one channel over a window.
    /// `scale` is `"1MIN"`, `"1H"`, or `"1D"`.
    fn chart_usage(
        &self,
        device_gid: u64,
        channel: &str,
        start: &str,
        end: &str,
        scale: &str,
    ) -> Result<Value, EmporiaError> {
        let gid = device_gid.to_string();
        self.get(
            "AppAPI",
            &[
                ("apiMethod", "getChartUsage"),
                ("deviceGid", &gid),
                ("channel", channel),
                ("start", start),
                ("end", end),
                ("scale", scale),
                ("energyUnit", "KilowattHours"),
            ],
        )
    }
}

#[derive(Debug)]
enum EmporiaError {
    Unauthorized,
    Other(String),
}

impl std::fmt::Display for EmporiaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmporiaError::Unauthorized => write!(f, "unauthorized (401)"),
            EmporiaError::Other(m) => write!(f, "{m}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor.

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SyncState {
    /// Per-channel watermark: `"{deviceGid}:{channelNum}"` → latest interval
    /// start as UTC epoch seconds (the last second we've stored).
    #[serde(default)]
    channels: BTreeMap<String, u64>,
}

fn load_state(vault: &Vault) -> SyncState {
    let path = vault.root().join(SYNC_FILE);
    if let Ok(bytes) = std::fs::read(&path) {
        serde_json::from_slice(&bytes).unwrap_or_default()
    } else {
        SyncState::default()
    }
}

fn save_state(vault: &Vault, state: &SyncState) -> Result<()> {
    let path = vault.root().join(SYNC_FILE);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    crate::store::write_json_atomic(&path, state)
}

// ---------------------------------------------------------------------------
// Energy row writer (home.energy unbound draft — raw JSONL, not a Rust type).

/// Raw line wrapper that holds the partition timestamp separately so the
/// JsonlStream can key on `ts` without it appearing twice in the file.
#[derive(Serialize)]
struct EnergyLine {
    #[serde(skip)]
    partition_ts: String,
    #[serde(flatten)]
    data: Value,
}

/// Append energy interval rows (home.energy draft schema) to the energy
/// directory, deduplicating by `guid`.
fn append_energy(vault: &Vault, rows: &[Value]) -> Result<u64> {
    if rows.is_empty() {
        return Ok(0);
    }
    let stream = vault.stream(ENERGY_DIR, Partition::Month);
    let mut seen: HashSet<String> = HashSet::new();
    for key in stream.partitions()? {
        for v in stream.read::<Value>(&key)? {
            if let Some(g) = v.get("guid").and_then(Value::as_str) {
                seen.insert(g.to_string());
            }
        }
    }
    let fresh: Vec<EnergyLine> = rows
        .iter()
        .filter(|r| {
            let g = r.get("guid").and_then(Value::as_str).unwrap_or("");
            g.is_empty() || seen.insert(g.to_string())
        })
        .map(|r| {
            let ts = r.get("ts").and_then(Value::as_str).unwrap_or("").to_string();
            EnergyLine { partition_ts: ts, data: r.clone() }
        })
        .collect();
    let n = fresh.len() as u64;
    stream.append(&fresh, |r| &r.partition_ts)?;
    Ok(n)
}

/// Raw line for a poll response.
#[derive(Serialize)]
struct RawLine {
    ts: String,
    #[serde(flatten)]
    data: Value,
}

fn append_raw(vault: &Vault, ts: &str, data: Value) -> Result<()> {
    let stream = vault.stream(RAW_DIR, Partition::Month);
    let line = RawLine { ts: ts.to_string(), data };
    stream.append(&[line], |r| &r.ts)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Energy interval helpers.

/// Convert a chart usage response into home.energy-shaped rows.
///
/// `firstUsageInstant` (RFC3339/ISO) marks the start of the first bucket;
/// `usageList` is a Vec of f64 kWh values, one per scale bucket.  Missing
/// buckets are `null` in the JSON (→ None in the list) — we skip them.
fn chart_to_rows(
    resp: &Value,
    device_gid: u64,
    channel: &str,
    circuit_name: &str,
    scale: &str,
) -> Vec<Value> {
    let interval_secs: u64 = match scale {
        "1S" => 1,
        "1MIN" => 60,
        "15MIN" => 900,
        "1H" => 3600,
        "1D" => 86400,
        _ => 0,
    };
    let first_str = match resp.get("firstUsageInstant").and_then(Value::as_str) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let first_dt = match DateTime::parse_from_rfc3339(first_str) {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(_) => return Vec::new(),
    };
    let usage_list = match resp.get("usageList").and_then(Value::as_array) {
        Some(a) => a,
        None => return Vec::new(),
    };

    let mut rows = Vec::new();
    for (i, entry) in usage_list.iter().enumerate() {
        let kwh = match entry.as_f64() {
            Some(v) if v.is_finite() => v,
            _ => continue, // null / NaN → skip
        };
        let bucket_start =
            first_dt + chrono::Duration::seconds(interval_secs as i64 * i as i64);
        let ts = bucket_start.to_rfc3339();
        let epoch_secs = bucket_start.timestamp() as u64;
        let guid = format!("emporia:{device_gid}:{channel}:{epoch_secs}:{scale}");
        let mut row = json!({
            "ts": ts,
            "source": "emporia",
            "device": device_gid.to_string(),
            "circuit": circuit_name,
            "kwh": kwh,
            "direction": "consumption",
            "guid": guid,
        });
        if interval_secs > 0 {
            row.as_object_mut()
                .unwrap()
                .insert("interval_secs".into(), interval_secs.into());
        }
        rows.push(row);
    }
    rows
}

// ---------------------------------------------------------------------------
// Main pull logic.

/// Resolve and refresh the id_token from the secret store.  Returns the live
/// client or an error if no credentials are stored / auth fails.
fn resolve_client(vault: &Vault) -> Result<EmporiaClient> {
    let mut tok = vault
        .load_sync_token(SERVICE)?
        .context("Emporia: no credentials stored — connect your account first")?;

    // Refresh if expired or about to expire.
    if tok.expired() {
        let refresh = tok
            .refresh_token
            .as_deref()
            .context("Emporia: access token expired and no refresh token — reconnect")?;
        let fresh = refresh_tokens(refresh).context("Emporia token refresh failed")?;
        // Preserve the username label stored in token_type.
        let label = tok.token_type.clone();
        tok = TokenSet { token_type: label, ..fresh };
        vault.save_sync_token(SERVICE, &tok)?;
    }

    Ok(EmporiaClient { id_token: tok.access_token })
}


/// Emporia data-retention boundaries (seconds before now).
///
/// Emporia retains:
///   - 1-minute data: ~7 days
///   - 15-minute data: ~1 year
///   - Hourly / daily: long-term
///
/// Two-tier backfill strategy: use `1H` scale for any window older than
/// MIN_CUTOFF_SECS (7 days), and `1MIN` only for the recent window within
/// that boundary.  This ensures first-sync backfills the full 30-day initial
/// window rather than only retrieving data that Emporia still has at 1-minute
/// resolution.
///
/// Timestamps are emitted in UTC (RFC 3339, `+00:00`).  The Emporia API
/// returns `firstUsageInstant` in UTC and the bucket offsets are uniform
/// intervals, so UTC is the natural and unambiguous representation here.
/// Sibling home collectors that have a device-local timezone (e.g.
/// ambient-weather) use local time; Emporia does not expose a per-device
/// locale in the public API response.
const MINUTE_RETENTION_SECS: u64 = 7 * 24 * 3600; // 7 days

/// Fetch one time window at a given scale and append rows, returning the count
/// of new rows written.  Also advances the `cursor_key` watermark in `state`
/// to `end_epoch`, even when zero rows are written, so idle/zero-usage
/// channels slide forward rather than re-requesting the same window forever.
fn fetch_and_store(
    client: &EmporiaClient,
    vault: &Vault,
    ts_str: &str,
    state: &mut SyncState,
    device_gid: u64,
    channel_num: &str,
    circuit_name: &str,
    cursor_key: &str,
    start_epoch: u64,
    end_epoch: u64,
    scale: &str,
    interval_advance: u64,
) -> Result<u64> {
    let start_dt = Utc.timestamp_opt(start_epoch as i64, 0).single()
        .unwrap_or_else(Utc::now);
    let end_dt = Utc.timestamp_opt(end_epoch as i64, 0).single()
        .unwrap_or_else(Utc::now);
    let start_str = start_dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let end_str = end_dt.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let resp = match client.chart_usage(device_gid, channel_num, &start_str, &end_str, scale) {
        Ok(r) => r,
        Err(e) => {
            let _ = append_raw(vault, ts_str, json!({
                "error": format!("{e}"),
                "device_gid": device_gid,
                "channel_num": channel_num,
                "scale": scale,
            }));
            return Ok(0);
        }
    };

    let _ = append_raw(vault, ts_str, json!({
        "device_gid": device_gid,
        "channel_num": channel_num,
        "circuit_name": circuit_name,
        "scale": scale,
        "response": resp.clone(),
    }));

    let rows = chart_to_rows(&resp, device_gid, channel_num, circuit_name, scale);

    // Derive new watermark from the GUIDs of returned rows; fall back to
    // end_epoch so idle/zero-usage channels always advance.
    let guid_wm = rows
        .iter()
        .filter_map(|r| {
            r.get("guid")
                .and_then(Value::as_str)
                .and_then(|g| g.split(':').nth(3))
                .and_then(|s| s.parse::<u64>().ok())
        })
        .max();

    let n = append_energy(vault, &rows)?;

    // Always advance the watermark to at least end_epoch — this prevents
    // idle channels (zero usage) from re-requesting the same window on every
    // poll.  When we did store rows we advance past the last stored bucket by
    // one interval to avoid re-fetching it.
    let new_wm = if let Some(wm) = guid_wm {
        wm + interval_advance
    } else {
        end_epoch
    };
    // Only move the watermark forward, never backward.
    let prev = state.channels.get(cursor_key).copied().unwrap_or(0);
    if new_wm > prev {
        state.channels.insert(cursor_key.to_string(), new_wm);
    }

    Ok(n)
}

/// Top-level pull function.  Returns the count of new energy rows written.
///
/// Two-tier backfill: on first sync (watermark ~30 days old) the deep window
/// (>7 days ago) is fetched at 1H resolution (long-term retention), and the
/// recent ≤7-day window is fetched at 1MIN resolution (short-term retention).
/// Subsequent incremental polls only request the 1MIN window.
fn pull(vault: &Vault) -> Result<u64> {
    let client = resolve_client(vault)?;

    // Fetch device list.
    let devices_resp = client
        .devices()
        .map_err(|e| anyhow::anyhow!("device list: {e}"))?;

    let now_utc = Utc::now();
    let now_epoch = now_utc.timestamp() as u64;
    let ts_str = now_utc.to_rfc3339();

    // Persist raw device list.
    let _ = append_raw(vault, &ts_str, devices_resp.clone());

    let mut state = load_state(vault);
    let mut total_new: u64 = 0;

    // Boundary between deep (hourly) and recent (minute) resolution.
    let minute_boundary = now_epoch.saturating_sub(MINUTE_RETENTION_SECS);

    // Walk each device and each channel.
    let devices = match devices_resp.get("devices").and_then(Value::as_array) {
        Some(d) => d,
        None => return Ok(0),
    };
    for device in devices {
        let device_gid = match device.get("deviceGid").and_then(Value::as_u64) {
            Some(g) => g,
            None => continue,
        };
        let channels = match device.get("channels").and_then(Value::as_array) {
            Some(c) => c,
            None => continue,
        };
        for ch in channels {
            let channel_num = match ch.get("channelNum").and_then(Value::as_str) {
                Some(n) => n,
                None => continue,
            };
            let circuit_name = ch
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(channel_num);
            let cursor_key = format!("{device_gid}:{channel_num}");
            // Default watermark: 30 days ago for backfill on first connect.
            let watermark = *state.channels.entry(cursor_key.clone()).or_insert_with(|| {
                now_epoch.saturating_sub(30 * 86400)
            });

            // --- Two-tier fetch ---
            //
            // Tier 1 (deep backfill): watermark is older than the 7-day
            // 1-minute retention window → fetch at 1H up to the boundary.
            if watermark < minute_boundary {
                let n = fetch_and_store(
                    &client,
                    vault,
                    &ts_str,
                    &mut state,
                    device_gid,
                    channel_num,
                    circuit_name,
                    &cursor_key,
                    watermark,
                    minute_boundary,
                    "1H",
                    3600, // advance by one hour-interval
                )?;
                total_new += n;
            }

            // Re-read the (possibly updated) watermark for tier 2.
            let watermark2 = *state.channels.get(&cursor_key).unwrap_or(&watermark);
            // Clamp start to the minute_boundary so we don't re-request the
            // deep window at 1MIN even if tier-1 advanced less than expected.
            let start2 = watermark2.max(minute_boundary);

            // Tier 2 (incremental): fetch at 1MIN from start2 → now.
            let n = fetch_and_store(
                &client,
                vault,
                &ts_str,
                &mut state,
                device_gid,
                channel_num,
                circuit_name,
                &cursor_key,
                start2,
                now_epoch,
                "1MIN",
                60, // advance by one minute-interval
            )?;
            total_new += n;
        }
    }

    save_state(vault, &state)?;
    Ok(total_new)
}

// ---------------------------------------------------------------------------
// Tests.

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a unique temp vault for each test (unique per-test-name + pid).
    fn tmp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir()
            .join(format!("trove-emporia-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    // ---------------------------------------------------------------------------
    // Credential parsing tests.

    #[test]
    fn parse_credentials_valid() {
        let (u, p) = parse_credentials("user@example.com:s3cret").unwrap();
        assert_eq!(u, "user@example.com");
        assert_eq!(p, "s3cret");
    }

    #[test]
    fn parse_credentials_password_with_colon() {
        // Password may contain `:` — only the first colon is the separator.
        let (u, p) = parse_credentials("user@example.com:pass:word").unwrap();
        assert_eq!(u, "user@example.com");
        assert_eq!(p, "pass:word");
    }

    #[test]
    fn parse_credentials_empty_fails() {
        assert!(parse_credentials("").is_err());
    }

    #[test]
    fn parse_credentials_no_colon_fails() {
        assert!(parse_credentials("nodivider").is_err());
    }

    #[test]
    fn parse_credentials_missing_password_fails() {
        assert!(parse_credentials("user@example.com:").is_err());
    }

    // ---------------------------------------------------------------------------
    // SRP math tests (offline — no network).

    #[test]
    fn pad_hex_small_value() {
        // A small value (< 128 in decimal) should not need a leading zero byte.
        let n = BigUint::from(0x42u32);
        assert_eq!(pad_hex(&n), "42");
    }

    #[test]
    fn pad_hex_high_bit_set() {
        // Value ≥ 0x80 in the leading byte needs a leading 00 byte.
        let n = BigUint::from(0x80u32);
        let h = pad_hex(&n);
        assert!(h.starts_with("00"), "expected leading 00, got {h}");
    }

    #[test]
    fn compute_k_stable() {
        // k is deterministic from N and g; round-tripping it should not panic.
        let n = BigUint::parse_bytes(N_HEX.replace('\n', "").as_bytes(), 16).unwrap();
        let g = BigUint::parse_bytes(G_HEX.as_bytes(), 16).unwrap();
        let k = compute_k(&n, &g);
        assert!(k > BigUint::from(0u32));
    }

    #[test]
    fn hkdf_16_is_deterministic() {
        let key = hkdf_16(b"session_key_material", b"srp_u_value");
        let key2 = hkdf_16(b"session_key_material", b"srp_u_value");
        assert_eq!(key, key2);
    }

    #[test]
    fn hkdf_16_outputs_16_bytes() {
        let key = hkdf_16(b"test", b"salt");
        assert_eq!(key.len(), 16);
    }

    #[test]
    fn cognito_timestamp_format() {
        // Should look like "Mon Jan 15 14:30:45 UTC 2024" — basic sanity.
        let ts = cognito_timestamp();
        assert!(ts.contains("UTC"), "timestamp missing UTC: {ts}");
        assert_eq!(ts.split_whitespace().count(), 6);
    }

    // ---------------------------------------------------------------------------
    // Cursor state persistence.

    #[test]
    fn state_round_trips() {
        let vault = tmp_vault("state_round_trips");
        let mut s = SyncState::default();
        s.channels.insert("12345:1".to_string(), 1718000000u64);
        save_state(&vault, &s).unwrap();
        let s2 = load_state(&vault);
        assert_eq!(s2.channels.get("12345:1"), Some(&1718000000u64));
    }

    // ---------------------------------------------------------------------------
    // Chart-to-rows conversion (fixture-driven, offline).

    /// Fixture: minimal getChartUsage response for channel "1" on device 12345.
    fn chart_fixture() -> Value {
        json!({
            "firstUsageInstant": "2026-06-10T13:00:00Z",
            "usageList": [0.013, 0.017, null, 0.021]
        })
    }

    #[test]
    fn chart_to_rows_basic() {
        let resp = chart_fixture();
        let rows = chart_to_rows(&resp, 12345, "1", "Dryer", "1MIN");
        // null entry skipped → 3 rows.
        assert_eq!(rows.len(), 3);
        let first = &rows[0];
        assert_eq!(first["source"], "emporia");
        assert_eq!(first["device"], "12345");
        assert_eq!(first["circuit"], "Dryer");
        assert!((first["kwh"].as_f64().unwrap() - 0.013).abs() < 1e-9);
        assert_eq!(first["direction"], "consumption");
        // ts starts at firstUsageInstant.
        assert!(first["ts"].as_str().unwrap().starts_with("2026-06-10T13:00:00"));
    }

    #[test]
    fn chart_to_rows_guid_stable() {
        let resp = chart_fixture();
        let rows1 = chart_to_rows(&resp, 12345, "1", "Dryer", "1MIN");
        let rows2 = chart_to_rows(&resp, 12345, "1", "Dryer", "1MIN");
        // GUIDs must be identical across runs for dedup to work.
        assert_eq!(rows1[0]["guid"], rows2[0]["guid"]);
    }

    #[test]
    fn chart_to_rows_guid_format() {
        let resp = chart_fixture();
        let rows = chart_to_rows(&resp, 12345, "1", "Dryer", "1MIN");
        let guid = rows[0]["guid"].as_str().unwrap();
        // Format: "emporia:{deviceGid}:{channelNum}:{epochSecs}:{scale}"
        let parts: Vec<&str> = guid.split(':').collect();
        assert_eq!(parts[0], "emporia");
        assert_eq!(parts[1], "12345");
        assert_eq!(parts[2], "1");
        assert!(parts[3].parse::<u64>().is_ok(), "epoch secs: {}", parts[3]);
        assert_eq!(parts[4], "1MIN");
    }

    #[test]
    fn chart_to_rows_interval_secs() {
        let resp = chart_fixture();
        let rows = chart_to_rows(&resp, 12345, "1", "Dryer", "1MIN");
        assert_eq!(rows[0]["interval_secs"], 60);
    }

    #[test]
    fn chart_to_rows_empty_on_no_list() {
        let resp = json!({ "firstUsageInstant": "2026-06-10T13:00:00Z" });
        let rows = chart_to_rows(&resp, 12345, "1", "Dryer", "1MIN");
        assert!(rows.is_empty());
    }

    #[test]
    fn chart_to_rows_skips_all_nulls() {
        let resp = json!({
            "firstUsageInstant": "2026-06-10T13:00:00Z",
            "usageList": [null, null, null]
        });
        let rows = chart_to_rows(&resp, 12345, "1", "Dryer", "1MIN");
        assert!(rows.is_empty());
    }

    // ---------------------------------------------------------------------------
    // Energy append / dedup (vault I/O, unique temp dir).

    #[test]
    fn append_energy_dedup() {
        let vault = tmp_vault("append_energy_dedup");
        // guid epoch: 2026-06-10T13:00:00Z = 1781096400
        let rows = vec![
            json!({
                "ts": "2026-06-10T13:00:00Z",
                "source": "emporia",
                "device": "12345",
                "circuit": "Dryer",
                "kwh": 0.013,
                "interval_secs": 60,
                "direction": "consumption",
                "guid": "emporia:12345:1:1781096400:1MIN",
            }),
        ];
        let n1 = append_energy(&vault, &rows).unwrap();
        assert_eq!(n1, 1);
        // Second append with same guid should be deduped.
        let n2 = append_energy(&vault, &rows).unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn append_energy_two_rows() {
        let vault = tmp_vault("append_energy_two_rows");
        // guid epochs: 2026-06-10T13:00:00Z = 1781096400,
        //              2026-06-10T13:01:00Z = 1781096460
        let rows = vec![
            json!({
                "ts": "2026-06-10T13:00:00Z", "source": "emporia",
                "device": "12345", "circuit": "Dryer", "kwh": 0.013,
                "interval_secs": 60, "direction": "consumption",
                "guid": "emporia:12345:1:1781096400:1MIN",
            }),
            json!({
                "ts": "2026-06-10T13:01:00Z", "source": "emporia",
                "device": "12345", "circuit": "Dryer", "kwh": 0.017,
                "interval_secs": 60, "direction": "consumption",
                "guid": "emporia:12345:1:1781096460:1MIN",
            }),
        ];
        let n = append_energy(&vault, &rows).unwrap();
        assert_eq!(n, 2);
    }

    // ---------------------------------------------------------------------------
    // Fixture JSON round-trip (schema sanity).

    #[test]
    fn energy_row_matches_home_energy_draft_schema() {
        // A row produced by chart_to_rows must round-trip via serde_json::Value.
        let resp = chart_fixture();
        let rows = chart_to_rows(&resp, 12345, "1", "Dryer", "1MIN");
        let j = serde_json::to_string(&rows[0]).unwrap();
        let v: Value = serde_json::from_str(&j).unwrap();
        // Required fields per the home.energy draft.
        assert!(v.get("ts").is_some());
        assert!(v.get("source").is_some());
        // Optional fields that should be present.
        assert!(v.get("kwh").is_some());
        assert!(v.get("circuit").is_some());
        assert!(v.get("direction").is_some());
        assert!(v.get("guid").is_some());
    }
}
