//! trove-mcp: a read-only MCP server over a Trove vault.
//!
//! The window is Trove's visual layer; this is its question layer. Any MCP
//! client (Claude Code, Claude Desktop, …) gets the same tools over the same
//! files: what sources exist, what streams hold data, a page of raw records,
//! the spec for a domain's record shape, notes search, and the one typed
//! aggregate that exists so far (health series). Every read is a thin
//! wrapper over a `trove-core` read path — nothing here parses vault files
//! itself — and every caller-supplied path goes through the vault's jail
//! (no escapes, `.trove/` never readable). No tool writes to the vault.

pub mod server;
pub mod spec;

pub use server::TroveServer;
