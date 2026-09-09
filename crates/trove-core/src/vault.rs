use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// Top-level folders every vault contains.
const LAYOUT: &[&str] = &[
    "artifacts",
    "health",
    "activity",
    "browser",
    "correspondence",
    "tasks",
    "inbox",
    ".trove",
];

/// A Trove vault: a plain folder on disk holding all of the user's data.
pub struct Vault {
    root: PathBuf,
}

/// Extensions a user document may have — what can be imported and edited.
/// Deliberately narrow for now; arbitrary types (with open-in-default-app
/// for ones we can't render) come later.
const ARTIFACT_EXTS: &[&str] = &["md", "txt"];

#[derive(Debug, Clone, Serialize)]
#[cfg_attr(feature = "specta", derive(specta::Type))]
pub struct ArtifactMeta {
    /// Path relative to the vault root, e.g. "artifacts/2026-06-10-thoughts.md"
    pub path: String,
    /// First `# ` heading if present, otherwise the filename.
    pub title: String,
    /// Last modified time, seconds since the Unix epoch.
    pub modified: u64,
}

impl Vault {
    /// Default vault location: ~/Trove
    pub fn default_root() -> PathBuf {
        dirs::home_dir()
            .expect("no home directory")
            .join("Trove")
    }

    /// Open the vault at `root`, creating it and its layout if missing.
    pub fn open_or_create(root: PathBuf) -> Result<Self> {
        // One-time migration: notes/ became artifacts/ (authored notes plus
        // imported documents). Merge rather than rename — another process may
        // already have created an empty artifacts/ via the layout loop.
        let old_notes = root.join("notes");
        let artifacts = root.join("artifacts");
        if old_notes.is_dir() {
            fs::create_dir_all(&artifacts).context("creating artifacts/")?;
            for entry in fs::read_dir(&old_notes).context("reading notes/")? {
                let entry = entry.context("reading notes/ entry")?;
                let name = entry.file_name();
                let mut dest = artifacts.join(&name);
                let mut n = 1;
                while dest.exists() {
                    n += 1;
                    let p = Path::new(&name);
                    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
                    dest = artifacts.join(match p.extension() {
                        Some(ext) => format!("{stem}-{n}.{}", ext.to_string_lossy()),
                        None => format!("{stem}-{n}"),
                    });
                }
                fs::rename(entry.path(), &dest)
                    .with_context(|| format!("migrating {:?} to artifacts/", name))?;
            }
            fs::remove_dir(&old_notes).context("removing migrated notes/")?;
        }
        for dir in LAYOUT {
            fs::create_dir_all(root.join(dir))
                .with_context(|| format!("creating vault folder {dir}"))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a vault-relative path, refusing anything that escapes the root.
    ///
    /// This is the *internal* resolver: it jails path escapes (absolute paths,
    /// `..`) but deliberately still permits `.trove/` because core writers
    /// legitimately live there (sync cursors, mappings, live sidecars). For any
    /// caller-supplied / untrusted path (IPC-reachable read paths), use
    /// [`Vault::resolve_user`], which additionally jails `.trove/`.
    pub(crate) fn resolve(&self, rel: &str) -> Result<PathBuf> {
        let p = Path::new(rel);
        if p.is_absolute() || p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
            bail!("path escapes vault: {rel}");
        }
        Ok(self.root.join(p))
    }

    /// Resolve an **untrusted, caller-supplied** vault-relative path. Like
    /// [`Vault::resolve`], but additionally jails the machine-artifact tree: no
    /// resolved path may contain a `.trove` segment. This is the resolver every
    /// IPC-reachable read path must use, so `.trove/` secrets (BYO API keys,
    /// OAuth tokens, mapping artifacts) are never readable through a
    /// caller-controlled path.
    ///
    /// The `.trove` check is on the resolved path's components (so a `./`
    /// prefix, which `Path` normalizes away, cannot smuggle it past), and is
    /// case-insensitive because the default macOS (APFS) filesystem is
    /// case-insensitive — `.Trove` names the very same directory as `.trove`.
    pub(crate) fn resolve_user(&self, rel: &str) -> Result<PathBuf> {
        let abs = self.resolve(rel)?;
        let tail = abs.strip_prefix(&self.root).unwrap_or(&abs);
        for comp in tail.components() {
            if let std::path::Component::Normal(c) = comp {
                if c.to_string_lossy().eq_ignore_ascii_case(".trove") {
                    bail!(".trove/ is not accessible via a caller-supplied path: {rel}");
                }
            }
        }
        Ok(abs)
    }

    /// All artifacts (authored notes + imported documents) under artifacts/,
    /// newest first.
    pub fn list_artifacts(&self) -> Result<Vec<ArtifactMeta>> {
        let artifacts_dir = self.root.join("artifacts");
        let mut artifacts = Vec::new();
        collect_artifacts(&artifacts_dir, &mut artifacts)?;
        for artifact in &mut artifacts {
            // Make paths vault-relative for the frontend.
            artifact.path = format!(
                "artifacts/{}",
                Path::new(&artifact.path)
                    .strip_prefix(&artifacts_dir)
                    .unwrap_or(Path::new(&artifact.path))
                    .to_string_lossy()
            );
        }
        artifacts.sort_by(|a, b| b.modified.cmp(&a.modified));
        Ok(artifacts)
    }

    pub fn read_artifact(&self, rel: &str) -> Result<String> {
        // Caller-supplied path (the `read_artifact` IPC command forwards it
        // verbatim), so jail `.trove/` too — reading a document never needs it.
        let path = self.resolve_user(rel)?;
        fs::read_to_string(&path).with_context(|| format!("reading {rel}"))
    }

    pub fn write_artifact(&self, rel: &str, content: &str) -> Result<()> {
        let path = self.resolve(rel)?;
        if !is_artifact_file(&path) {
            bail!("artifacts must be .md or .txt files");
        }
        fs::write(&path, content).with_context(|| format!("writing {rel}"))
    }

    /// Create a new markdown artifact titled `title`, returning its metadata.
    /// Filenames are slugified and deduplicated.
    pub fn create_artifact(&self, title: &str) -> Result<ArtifactMeta> {
        let slug = slugify(title);
        let artifacts_dir = self.root.join("artifacts");
        let filename = dedupe_filename(&artifacts_dir, &slug, "md");
        let rel = format!("artifacts/{filename}");
        let content = format!("# {title}\n\n");
        self.write_artifact(&rel, &content)?;
        Ok(ArtifactMeta {
            path: rel,
            title: title.to_string(),
            modified: now_epoch(),
        })
    }

    pub fn delete_artifact(&self, rel: &str) -> Result<()> {
        let path = self.resolve(rel)?;
        if !path.starts_with(self.root.join("artifacts")) {
            bail!("can only delete files under artifacts/");
        }
        fs::remove_file(&path).with_context(|| format!("deleting {rel}"))
    }

    /// Copy user-dropped files into artifacts/ as-is (no format conversion),
    /// keeping original filenames and deduplicating collisions. Only .md and
    /// .txt are accepted for now.
    pub fn import_artifacts(&self, sources: &[PathBuf]) -> Result<Vec<ArtifactMeta>> {
        let artifacts_dir = self.root.join("artifacts");
        let mut imported = Vec::new();
        for src in sources {
            if !is_artifact_file(src) {
                bail!(
                    "unsupported file type: {} (only .md and .txt for now)",
                    src.display()
                );
            }
            let stem = src
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "untitled".into());
            let ext = src
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_else(|| "txt".into());
            let filename = dedupe_filename(&artifacts_dir, &stem, &ext);
            let dest = artifacts_dir.join(&filename);
            fs::copy(src, &dest).with_context(|| format!("importing {}", src.display()))?;
            imported.push(ArtifactMeta {
                title: artifact_title(&dest),
                path: format!("artifacts/{filename}"),
                modified: now_epoch(),
            });
        }
        Ok(imported)
    }

    /// Artifacts whose filename, title, or content contains `query`
    /// (case-insensitive), newest first. A full scan is fine at this scale;
    /// real indexed search arrives with SQLite.
    pub fn search_artifacts(&self, query: &str) -> Result<Vec<ArtifactMeta>> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return self.list_artifacts();
        }
        Ok(self
            .list_artifacts()?
            .into_iter()
            .filter(|a| {
                a.title.to_lowercase().contains(&q)
                    || a.path.to_lowercase().contains(&q)
                    || self
                        .read_artifact(&a.path)
                        .map(|c| c.to_lowercase().contains(&q))
                        .unwrap_or(false)
            })
            .collect())
    }

    /// Replace this browser host's live sidecar with its currently-open spans.
    /// `key` identifies the writer (the host pid) so concurrent profile hosts
    /// don't clobber each other. The write is atomic (temp + rename) so a
    /// reader never sees a half-written file. Sidecars are ephemeral runtime
    /// state under `.trove/live/`, NOT vault data — readers tolerate them
    /// missing and they're safe to delete.
    pub fn write_browser_live(
        &self,
        browser: &str,
        key: &str,
        spans: &[crate::browser_ext::LiveSpan],
    ) -> Result<()> {
        let dir = self.root.join(LIVE_DIR);
        fs::create_dir_all(&dir).context("creating live dir")?;
        let state = crate::browser_ext::LiveState {
            browser: browser.to_string(),
            updated: now_epoch(),
            spans: spans.to_vec(),
        };
        let tmp = dir.join(format!("browser-{key}.json.tmp"));
        let path = dir.join(format!("browser-{key}.json"));
        fs::write(&tmp, serde_json::to_vec(&state)?)
            .with_context(|| format!("writing live sidecar {key}"))?;
        fs::rename(&tmp, &path).with_context(|| format!("publishing live sidecar {key}"))?;
        Ok(())
    }

    /// Remove this host's live sidecar — call when it disconnects so the UI
    /// stops showing its "watching now" rows immediately rather than waiting
    /// for the staleness TTL. Absent file is success.
    pub fn clear_browser_live(&self, key: &str) -> Result<()> {
        let path = self.root.join(LIVE_DIR).join(format!("browser-{key}.json"));
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("clearing live sidecar {key}")),
        }
    }

    /// Every currently-open browser span across all live hosts. Sidecars older
    /// than [`LIVE_TTL_SECS`] (the host died without clearing) are skipped and
    /// best-effort deleted. Empty when nothing is being watched right now.
    pub fn read_browser_live(&self) -> Result<Vec<crate::browser_ext::LiveSpan>> {
        let dir = self.root.join(LIVE_DIR);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).context("reading live dir"),
        };
        let now = now_epoch();
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue; // skip *.json.tmp and anything else
            }
            let Ok(body) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(state) = serde_json::from_str::<crate::browser_ext::LiveState>(&body) else {
                continue;
            };
            if now.saturating_sub(state.updated) > LIVE_TTL_SECS {
                let _ = fs::remove_file(&path); // stale; best-effort cleanup
                continue;
            }
            out.extend(state.spans);
        }
        Ok(out)
    }
}

/// Ephemeral live-state sidecars live here — "what's open/playing right now",
/// written by each browser native-messaging host and read by the UI. NOT vault
/// data: rebuildable, safe to delete, never read back into the day JSONL.
const LIVE_DIR: &str = ".trove/live";

/// A live sidecar older than this is stale (its host died without clearing it)
/// and ignored — a few snapshot intervals past the extension's cadence.
const LIVE_TTL_SECS: u64 = 30;

fn is_artifact_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| ARTIFACT_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// First `stem.ext` filename that doesn't already exist in `dir`.
fn dedupe_filename(dir: &Path, stem: &str, ext: &str) -> String {
    let mut filename = format!("{stem}.{ext}");
    let mut n = 1;
    while dir.join(&filename).exists() {
        n += 1;
        filename = format!("{stem}-{n}.{ext}");
    }
    filename
}

fn collect_artifacts(dir: &Path, out: &mut Vec<ArtifactMeta>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_artifacts(&path, out)?;
        } else if is_artifact_file(&path) {
            let modified = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            out.push(ArtifactMeta {
                title: artifact_title(&path),
                path: path.to_string_lossy().into_owned(),
                modified,
            });
        }
    }
    Ok(())
}

/// First `# ` heading, else the filename without extension.
fn artifact_title(path: &Path) -> String {
    if let Ok(content) = fs::read_to_string(path) {
        for line in content.lines().take(10) {
            if let Some(h) = line.strip_prefix("# ") {
                let h = h.trim();
                if !h.is_empty() {
                    return h.to_string();
                }
            }
        }
    }
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Untitled".into())
}

pub(crate) fn slugify(s: &str) -> String {
    let slug: String = s
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        "untitled".into()
    } else {
        slug
    }
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_vault(name: &str) -> Vault {
        let dir = std::env::temp_dir().join(format!("trove-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        Vault::open_or_create(dir).unwrap()
    }

    #[test]
    fn create_list_read_write_delete() {
        let v = temp_vault("crud");
        let meta = v.create_artifact("Hello World").unwrap();
        assert_eq!(meta.path, "artifacts/hello-world.md");

        let artifacts = v.list_artifacts().unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].title, "Hello World");

        v.write_artifact(&meta.path, "# Renamed\n\nbody").unwrap();
        assert_eq!(v.read_artifact(&meta.path).unwrap(), "# Renamed\n\nbody");
        assert_eq!(v.list_artifacts().unwrap()[0].title, "Renamed");

        v.delete_artifact(&meta.path).unwrap();
        assert!(v.list_artifacts().unwrap().is_empty());
    }

    #[test]
    fn rejects_path_escape() {
        let v = temp_vault("escape");
        assert!(v.read_artifact("../outside.md").is_err());
        assert!(v.read_artifact("/etc/passwd").is_err());
    }

    #[test]
    fn dedupes_filenames() {
        let v = temp_vault("dedupe");
        let a = v.create_artifact("Same Title").unwrap();
        let b = v.create_artifact("Same Title").unwrap();
        assert_ne!(a.path, b.path);
    }

    #[test]
    fn txt_artifacts_are_listed_and_writable() {
        let v = temp_vault("txt");
        v.write_artifact("artifacts/plain.txt", "just text").unwrap();
        let artifacts = v.list_artifacts().unwrap();
        assert_eq!(artifacts.len(), 1);
        assert_eq!(artifacts[0].title, "plain");
        assert!(v.write_artifact("artifacts/doc.pdf", "nope").is_err());
    }

    #[test]
    fn imports_md_and_txt_with_dedupe() {
        let v = temp_vault("import");
        let src_dir = std::env::temp_dir().join(format!("trove-test-import-src-{}", std::process::id()));
        let _ = fs::remove_dir_all(&src_dir);
        fs::create_dir_all(&src_dir).unwrap();
        let md = src_dir.join("dropped.md");
        let txt = src_dir.join("readme.txt");
        fs::write(&md, "# Dropped Doc\n\nbody").unwrap();
        fs::write(&txt, "plain body").unwrap();

        let metas = v.import_artifacts(&[md.clone(), txt.clone()]).unwrap();
        assert_eq!(metas.len(), 2);
        assert_eq!(metas[0].path, "artifacts/dropped.md");
        assert_eq!(metas[0].title, "Dropped Doc");
        assert_eq!(metas[1].path, "artifacts/readme.txt");
        // Source files are copied, not moved.
        assert!(md.exists());
        // Re-importing the same file dedupes the destination name.
        let again = v.import_artifacts(&[md.clone()]).unwrap();
        assert_eq!(again[0].path, "artifacts/dropped-2.md");

        let pdf = src_dir.join("doc.pdf");
        fs::write(&pdf, "x").unwrap();
        assert!(v.import_artifacts(&[pdf]).is_err());
    }

    #[test]
    fn search_matches_title_filename_and_content() {
        let v = temp_vault("search");
        v.write_artifact("artifacts/alpha.md", "# Groceries\n\nbuy milk").unwrap();
        v.write_artifact("artifacts/beta.txt", "meeting agenda").unwrap();

        let by_content = v.search_artifacts("MILK").unwrap();
        assert_eq!(by_content.len(), 1);
        assert_eq!(by_content[0].path, "artifacts/alpha.md");

        let by_filename = v.search_artifacts("beta").unwrap();
        assert_eq!(by_filename.len(), 1);

        assert!(v.search_artifacts("zzz-nothing").unwrap().is_empty());
        // Empty query returns everything.
        assert_eq!(v.search_artifacts("  ").unwrap().len(), 2);
    }

    #[test]
    fn migrates_legacy_notes_dir() {
        let dir = std::env::temp_dir().join(format!("trove-test-{}-migrate", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("notes")).unwrap();
        fs::write(dir.join("notes/old.md"), "# Old Note\n").unwrap();

        let v = Vault::open_or_create(dir.clone()).unwrap();
        assert!(!dir.join("notes").exists());
        assert_eq!(v.list_artifacts().unwrap()[0].path, "artifacts/old.md");
    }

    #[test]
    fn migration_merges_into_existing_artifacts_dir() {
        let dir = std::env::temp_dir().join(format!("trove-test-{}-migrate-merge", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // An empty artifacts/ already created (e.g. by another process's
        // layout loop) must not block the migration; colliding names dedupe.
        fs::create_dir_all(dir.join("artifacts")).unwrap();
        fs::write(dir.join("artifacts/old.md"), "# Already Here\n").unwrap();
        fs::create_dir_all(dir.join("notes")).unwrap();
        fs::write(dir.join("notes/old.md"), "# Old Note\n").unwrap();
        fs::write(dir.join("notes/other.md"), "# Other\n").unwrap();

        let v = Vault::open_or_create(dir.clone()).unwrap();
        assert!(!dir.join("notes").exists());
        let mut paths: Vec<String> = v.list_artifacts().unwrap().into_iter().map(|a| a.path).collect();
        paths.sort();
        assert_eq!(paths, vec!["artifacts/old-2.md", "artifacts/old.md", "artifacts/other.md"]);
        assert_eq!(v.read_artifact("artifacts/old.md").unwrap(), "# Already Here\n");
        assert_eq!(v.read_artifact("artifacts/old-2.md").unwrap(), "# Old Note\n");
    }
}
