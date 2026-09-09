# OAuth app credentials: how Trove ships them

*Decided June 2026 (oauth worktree session, with David). Applies to every M5/OAuth
source. Do not relitigate without new facts (e.g. a paid sync tier actually existing).*

## The reality

A desktop app cannot keep an OAuth client secret confidential — anyone can extract
it from the binary (RFC 8252 treats native apps as public clients). The choice is
not "how to hide it" but which exposure model to accept. What a leaked client
secret exposes is Trove's *app identity* (consent-screen impersonation, shared
quota) — never any user's data: tokens are per-user, minted only after that user
consents in their own browser, and stored only on their machine.

## The model (rclone / gh-cli pattern)

Credentials resolve in a chain, implemented in `sync/oauth.rs` + each connector's
`connect()`: **explicitly entered → saved in `.trove/sync/` → compiled-in defaults**.

1. **Official release builds**: per-provider credentials baked in at compile time
   via `option_env!` (e.g. `TROVE_TICKTICK_CLIENT_ID` / `_SECRET`), supplied by
   GitHub Actions repository secrets in the release workflow. Downloaders get pure
   "just log in". Secrets never appear in the source tree.
2. **Source builds / fallback**: the Sync tab shows a bring-your-own-credentials
   form with per-provider setup steps. This is also the escape hatch if a shared
   credential is ever revoked, rate-limited, or awaiting verification — users are
   never bricked.
3. **Dev builds (David)**: same env vars, exported from `~/.zshrc`.

Per-provider marginal cost: two CI secrets. This scales to dozens of providers.

## What BYO solves that nothing else does

- **Verification-restricted scopes** (Gmail is the big one): Google's app
  verification/security assessment attaches to the registered OAuth app itself.
  Until/unless Trove's shared app passes it, BYO is the only path for those scopes.
- **Per-app rate limits** (e.g. Strava): a shared identity pools all users into
  one quota. BYO credentials are the only scalable answer for such providers.
- Providers supporting PKCE public clients (Spotify, Google "installed app" type)
  carry little shared-secret weight at all — `oauth.rs` already supports PKCE.

## Rejected: token-exchange proxy

A hosted service holding the secrets and performing the code-for-token exchange
was considered and rejected:

- The proxy would **see every user's access token in transit** — worse for the
  privacy posture than exposing our app identity.
- It becomes load-bearing infra in the login path (down = nobody can connect)
  and a standing ops/security cost, contradicting the standalone principle.
- It fixes none of the things that actually hurt at scale (verification,
  per-app quotas) and concentrates all secrets in one internet-facing target.

**Revisit condition:** if the far-future paid sync tier ever exists, a token
broker can piggyback on that infrastructure for users who opted into it. The
exchange step is isolated in `token_request()` (`sync/oauth.rs`), so adding a
broker mode later is a small, contained change.
