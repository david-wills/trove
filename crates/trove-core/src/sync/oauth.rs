//! Generic OAuth 2.0 authorization-code flow with a loopback redirect.
//!
//! The user supplies their *own* app credentials (registered in each
//! service's developer console) — Trove ships no client secrets. The flow:
//! bind a localhost listener, open the service's consent page in the
//! browser, catch the redirect, exchange the code for a token. PKCE is
//! supported for providers that want it (Google et al.); confidential
//! client_secret posting for those that don't (TickTick).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Static description of an OAuth service.
pub struct Provider {
    /// Stable slug; used in `.trove/sync/` file names. Lowercase ascii.
    pub service: &'static str,
    pub display_name: &'static str,
    pub auth_url: &'static str,
    pub token_url: &'static str,
    /// Space-separated scope string.
    pub scopes: &'static str,
    /// Loopback port for the redirect. Fixed per provider because most
    /// consoles require registering the exact redirect URI.
    pub redirect_port: u16,
    pub use_pkce: bool,
    /// Send client id/secret as HTTP Basic auth on token requests
    /// (otherwise they go in the form body).
    pub basic_auth: bool,
    /// App credentials compiled into the binary (via `option_env!` at the
    /// provider definition), so a build configured with them is pure
    /// "just log in" — no per-machine setup form.
    pub default_client_id: Option<&'static str>,
    pub default_client_secret: Option<&'static str>,
    /// Extra key/value pairs appended verbatim to the authorize URL. Most
    /// providers leave this empty; Google needs `access_type=offline` +
    /// `prompt=consent` to issue (and re-issue) a refresh token.
    pub extra_auth_params: &'static [(&'static str, &'static str)],
}

impl Provider {
    /// Must be registered verbatim in the service's developer console.
    pub fn redirect_uri(&self) -> String {
        format!("http://localhost:{}/callback", self.redirect_port)
    }

    /// Credentials baked in at build time, if any.
    pub fn default_credentials(&self) -> Option<AppCredentials> {
        Some(AppCredentials {
            client_id: self.default_client_id?.to_string(),
            client_secret: self.default_client_secret.map(str::to_string),
        })
    }
}

/// The user's own OAuth app, pasted in from the service's dev console.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppCredentials {
    pub client_id: String,
    pub client_secret: Option<String>,
}

/// A stored token. `expires_at` is absolute (epoch seconds) so it survives
/// restarts; raw `expires_in` from the wire is converted on receipt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: Option<String>,
    pub scope: Option<String>,
    pub expires_at: Option<u64>,
}

impl TokenSet {
    /// Expired or expiring within the next minute.
    pub fn expired(&self) -> bool {
        match self.expires_at {
            Some(at) => now_epoch() + 60 >= at,
            None => false,
        }
    }
}

/// Wire format of a token response (RFC 6749 §5.1).
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    token_type: Option<String>,
    scope: Option<String>,
    expires_in: Option<u64>,
}

impl TokenResponse {
    fn into_token_set(self, previous_refresh: Option<&str>) -> TokenSet {
        TokenSet {
            access_token: self.access_token,
            // Refresh responses often omit the refresh token; keep the old one.
            refresh_token: self
                .refresh_token
                .or_else(|| previous_refresh.map(str::to_string)),
            token_type: self.token_type,
            scope: self.scope,
            expires_at: self.expires_in.map(|secs| now_epoch() + secs),
        }
    }
}

/// An authorization in progress: listener bound, consent URL built.
pub struct OauthFlow {
    provider: &'static Provider,
    listener: TcpListener,
    state: String,
    pkce_verifier: Option<String>,
    authorize_url: String,
}

impl OauthFlow {
    /// Bind the loopback listener and build the consent URL. Fails fast if
    /// the port is taken (another connect attempt in flight).
    pub fn start(provider: &'static Provider, creds: &AppCredentials) -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", provider.redirect_port))
            .with_context(|| format!("binding localhost:{}", provider.redirect_port))?;
        Self::start_with_listener(provider, creds, listener)
    }

    /// Build the flow around an already-bound loopback listener. Production
    /// reaches this via `start`, which binds the provider's fixed
    /// `redirect_port` (it must match the pre-registered redirect URI). Tests
    /// bind an ephemeral `127.0.0.1:0` and call this directly so parallel
    /// `cargo test` runs never collide on a fixed port — the consent URL's port
    /// is irrelevant to the loopback round-trip, which connects to the
    /// listener's actual local address.
    pub(crate) fn start_with_listener(
        provider: &'static Provider,
        creds: &AppCredentials,
        listener: TcpListener,
    ) -> Result<Self> {
        listener.set_nonblocking(true).context("nonblocking listener")?;

        let state = URL_SAFE_NO_PAD.encode(random_bytes(24)?);
        let mut params = vec![
            ("response_type", "code".to_string()),
            ("client_id", creds.client_id.clone()),
            ("redirect_uri", provider.redirect_uri()),
            ("scope", provider.scopes.to_string()),
            ("state", state.clone()),
        ];
        let pkce_verifier = if provider.use_pkce {
            let verifier = URL_SAFE_NO_PAD.encode(random_bytes(48)?);
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            params.push(("code_challenge", challenge));
            params.push(("code_challenge_method", "S256".to_string()));
            Some(verifier)
        } else {
            None
        };
        for (k, v) in provider.extra_auth_params.iter().copied() {
            params.push((k, v.to_string()));
        }

        let query: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{k}={}", urlencode(v)))
            .collect();
        let authorize_url = format!("{}?{}", provider.auth_url, query.join("&"));

        Ok(Self {
            provider,
            listener,
            state,
            pkce_verifier,
            authorize_url,
        })
    }

    /// The consent page to open in the user's browser.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    /// Block until the browser redirects back with a code (verifying our
    /// `state`), then exchange it for a token.
    pub fn finish(self, creds: &AppCredentials, timeout: Duration) -> Result<TokenSet> {
        let code = self.wait_for_code(timeout)?;
        exchange_code(self.provider, creds, &code, self.pkce_verifier.as_deref())
    }

    fn wait_for_code(&self, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(code) = self.handle_request(stream)? {
                        return Ok(code);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        bail!("timed out waiting for the OAuth redirect");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(e).context("accepting OAuth redirect"),
            }
        }
    }

    /// Serve one HTTP request on the loopback listener. Returns the auth
    /// code if this was the redirect; None for noise (favicon etc.).
    fn handle_request(&self, mut stream: TcpStream) -> Result<Option<String>> {
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .context("read timeout")?;
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let path = match request.split_whitespace().nth(1) {
            Some(p) => p,
            None => return Ok(None),
        };

        let query = path.splitn(2, '?').nth(1).unwrap_or("");
        let mut code = None;
        let mut state = None;
        let mut error = None;
        for pair in query.split('&') {
            let mut kv = pair.splitn(2, '=');
            match (kv.next(), kv.next()) {
                (Some("code"), Some(v)) => code = Some(urldecode(v)),
                (Some("state"), Some(v)) => state = Some(urldecode(v)),
                (Some("error"), Some(v)) => error = Some(urldecode(v)),
                _ => {}
            }
        }

        if let Some(err) = error {
            respond(&mut stream, 200, &page("Connection refused", &err));
            bail!("{} authorization failed: {err}", self.provider.display_name);
        }
        let Some(code) = code else {
            respond(&mut stream, 404, "not found");
            return Ok(None);
        };
        if state.as_deref() != Some(self.state.as_str()) {
            respond(&mut stream, 400, &page("Connection refused", "state mismatch"));
            bail!("OAuth state mismatch — possible CSRF, aborting");
        }
        respond(
            &mut stream,
            200,
            &page(
                "Trove is connected",
                "You can close this tab and return to Trove.",
            ),
        );
        Ok(Some(code))
    }
}

/// Trade an authorization code for a token.
pub fn exchange_code(
    provider: &Provider,
    creds: &AppCredentials,
    code: &str,
    pkce_verifier: Option<&str>,
) -> Result<TokenSet> {
    let redirect_uri = provider.redirect_uri();
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &redirect_uri),
        ("scope", provider.scopes),
    ];
    if let Some(v) = pkce_verifier {
        form.push(("code_verifier", v));
    }
    token_request(provider, creds, form, None)
}

/// Refresh an expired token; the provider must have issued a refresh token.
pub fn refresh_token(
    provider: &Provider,
    creds: &AppCredentials,
    token: &TokenSet,
) -> Result<TokenSet> {
    let refresh = token
        .refresh_token
        .as_deref()
        .with_context(|| format!("{} issued no refresh token — reconnect", provider.display_name))?;
    let form = vec![("grant_type", "refresh_token"), ("refresh_token", refresh)];
    token_request(provider, creds, form, Some(refresh))
}

fn token_request<'a>(
    provider: &Provider,
    creds: &'a AppCredentials,
    mut form: Vec<(&'a str, &'a str)>,
    previous_refresh: Option<&str>,
) -> Result<TokenSet> {
    let mut req = ureq::post(provider.token_url)
        .timeout(Duration::from_secs(30))
        .set("Accept", "application/json");
    if provider.basic_auth {
        let secret = creds.client_secret.as_deref().unwrap_or("");
        let basic = STANDARD.encode(format!("{}:{}", creds.client_id, secret));
        req = req.set("Authorization", &format!("Basic {basic}"));
    } else {
        form.push(("client_id", &creds.client_id));
        if let Some(secret) = creds.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
    }
    let response: TokenResponse = req
        .send_form(&form)
        .map_err(describe_http_error)
        .with_context(|| format!("token request to {}", provider.token_url))?
        .into_json()
        .context("parsing token response")?;
    Ok(response.into_token_set(previous_refresh))
}

/// Flatten ureq's error into something with the response body in it —
/// OAuth servers put the useful part (`invalid_grant` etc.) in the body.
pub(crate) fn describe_http_error(err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            anyhow::anyhow!("HTTP {code}: {}", body.chars().take(500).collect::<String>())
        }
        other => anyhow::Error::from(other),
    }
}

/// Open `url` in the user's default browser.
pub fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open").arg(url).status();
    #[cfg(not(target_os = "macos"))]
    let status = std::process::Command::new("xdg-open").arg(url).status();
    let status = status.context("launching browser")?;
    if !status.success() {
        bail!("browser launch exited with {status}");
    }
    Ok(())
}

fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

fn page(title: &str, detail: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family:-apple-system,sans-serif;display:grid;place-items:center;height:100vh;margin:0\">\
         <div style=\"text-align:center\"><h1>{title}</h1><p>{detail}</p></div></body></html>"
    )
}

fn random_bytes(n: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).context("gathering randomness")?;
    Ok(buf)
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 3 <= bytes.len() => {
                if let Ok(byte) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(byte);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_PROVIDER: Provider = Provider {
        service: "test",
        display_name: "Test",
        auth_url: "https://example.com/oauth/authorize",
        token_url: "https://example.com/oauth/token",
        scopes: "a:read b:write",
        redirect_port: 39917,
        use_pkce: true,
        basic_auth: false,
        default_client_id: None,
        default_client_secret: None,
        extra_auth_params: &[],
    };

    #[test]
    fn authorize_url_carries_pkce_state_and_scopes() {
        let creds = AppCredentials {
            client_id: "my-client".into(),
            client_secret: None,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let flow = OauthFlow::start_with_listener(&TEST_PROVIDER, &creds, listener).unwrap();
        let url = flow.authorize_url();
        assert!(url.starts_with("https://example.com/oauth/authorize?"));
        assert!(url.contains("client_id=my-client"));
        assert!(url.contains("scope=a%3Aread%20b%3Awrite"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state="));
        assert!(url.contains(&urlencode("http://localhost:39917/callback")));
    }

    #[test]
    fn redirect_round_trip_returns_code() {
        // Ephemeral bind: each run gets a free port, so parallel `cargo test`
        // never collides on a fixed port (this test asserts no URL port).
        static P_RT: Provider = Provider {
            service: "test-rt",
            display_name: "TestRt",
            auth_url: "https://example.com/a",
            token_url: "https://example.com/t",
            scopes: "s",
            redirect_port: 39919,
            use_pkce: false,
            basic_auth: false,
            default_client_id: None,
            default_client_secret: None,
            extra_auth_params: &[],
        };
        let creds = AppCredentials {
            client_id: "c".into(),
            client_secret: None,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let flow = OauthFlow::start_with_listener(&P_RT, &creds, listener).unwrap();
        let state = flow.state.clone();

        let handle = std::thread::spawn(move || flow.wait_for_code(Duration::from_secs(5)));
        // Noise first (favicon), then the real redirect.
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /favicon.ico HTTP/1.1\r\n\r\n").unwrap();
        drop(s);
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            s,
            "GET /callback?code=the-code&state={state} HTTP/1.1\r\n\r\n"
        )
        .unwrap();
        let mut reply = String::new();
        let _ = s.read_to_string(&mut reply);
        assert!(reply.contains("200 OK"));

        assert_eq!(handle.join().unwrap().unwrap(), "the-code");
    }

    #[test]
    fn rejects_state_mismatch() {
        static P2: Provider = Provider {
            service: "test2",
            display_name: "Test2",
            auth_url: "https://example.com/a",
            token_url: "https://example.com/t",
            scopes: "s",
            redirect_port: 39918,
            use_pkce: false,
            basic_auth: false,
            default_client_id: None,
            default_client_secret: None,
            extra_auth_params: &[],
        };
        let creds = AppCredentials {
            client_id: "c".into(),
            client_secret: None,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let flow = OauthFlow::start_with_listener(&P2, &creds, listener).unwrap();
        let handle = std::thread::spawn(move || flow.wait_for_code(Duration::from_secs(5)));
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(s, "GET /callback?code=x&state=WRONG HTTP/1.1\r\n\r\n").unwrap();
        let mut reply = String::new();
        let _ = s.read_to_string(&mut reply);
        assert!(handle.join().unwrap().is_err());
    }

    #[test]
    fn token_expiry() {
        let live = TokenSet {
            access_token: "a".into(),
            refresh_token: None,
            token_type: None,
            scope: None,
            expires_at: Some(now_epoch() + 3600),
        };
        assert!(!live.expired());
        let stale = TokenSet { expires_at: Some(now_epoch() - 1), ..live.clone() };
        assert!(stale.expired());
        let unknown = TokenSet { expires_at: None, ..live };
        assert!(!unknown.expired());
    }

    #[test]
    fn url_coding() {
        assert_eq!(urlencode("a b:c"), "a%20b%3Ac");
        assert_eq!(urldecode("a%20b%3Ac"), "a b:c");
        assert_eq!(urldecode("plus+space"), "plus space");
        assert_eq!(urldecode("trailing%2"), "trailing%2");
    }
}
