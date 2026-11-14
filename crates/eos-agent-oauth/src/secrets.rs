//! Where a secret document is kept: a macOS Keychain item, a 0600 file, or memory.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use eos_agent_core::{BoxFuture, SecretStore};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use unicode_normalization::UnicodeNormalization;

/// A macOS Keychain generic-password item, through `security(1)`.
#[derive(Debug, Clone)]
pub struct KeychainSecrets {
    service: String,
}

impl KeychainSecrets {
    pub fn new(service: impl Into<String>) -> Self {
        Self { service: service.into() }
    }

    /// Claude Code's item: a store dir gets its own, suffixed with a hash of the dir.
    pub fn for_claude_store(store_dir: Option<&Path>) -> Self {
        let Some(dir) = store_dir else { return Self::new("Claude Code-credentials") };
        let normalized: String = dir.to_string_lossy().nfc().collect();
        let hash = hex::encode(Sha256::digest(normalized.as_bytes()));
        Self::new(format!("Claude Code-credentials-{}", &hash[..8]))
    }

    pub fn service(&self) -> &str {
        &self.service
    }

    /// The item's account attribute, so an update rewrites the same item.
    async fn account(&self) -> Option<String> {
        let out = tokio::process::Command::new("security")
            .args(["find-generic-password", "-s", &self.service])
            .output()
            .await
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().find(|l| l.trim_start().starts_with("\"acct\""))?;
        let value = line.split_once("=\"")?.1.strip_suffix('"')?;
        Some(value.to_owned())
    }
}

impl SecretStore for KeychainSecrets {
    fn read(&self) -> BoxFuture<'_, std::io::Result<Option<String>>> {
        Box::pin(async move {
            let out = tokio::process::Command::new("security")
                .args(["find-generic-password", "-s", &self.service, "-w"])
                .output()
                .await?;
            if !out.status.success() {
                return Ok(None);
            }
            let value = String::from_utf8_lossy(&out.stdout).trim().to_owned();
            Ok((!value.is_empty()).then_some(value))
        })
    }

    fn write<'a>(&'a self, value: &'a str) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            let account = self.account().await.unwrap_or_else(default_account);
            // Like Claude Code: the value goes in hex over stdin (`security -i`),
            // so it never shows in the process list; a line too long for
            // `security`'s reader goes as arguments instead.
            let hex_value = hex::encode(value);
            let line = format!(
                "add-generic-password -U -a \"{}\" -s \"{}\" -X \"{hex_value}\"\n",
                quoted(&account),
                quoted(&self.service)
            );
            let mut command = tokio::process::Command::new("security");
            let use_stdin = line.len() <= SECURITY_LINE_MAX;
            if use_stdin {
                command.arg("-i");
            } else {
                command.args(["add-generic-password", "-U", "-a", &account, "-s", &self.service, "-X", &hex_value]);
            }
            let mut child = command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()?;
            if let Some(mut stdin) = child.stdin.take()
                && use_stdin
            {
                stdin.write_all(line.as_bytes()).await?;
            }
            let out = child.wait_with_output().await?;
            if out.status.success() {
                Ok(())
            } else {
                Err(std::io::Error::other(format!(
                    "security add-generic-password failed: {}",
                    String::from_utf8_lossy(&out.stderr).lines().next().unwrap_or_default()
                )))
            }
        })
    }
}

/// Longest command line Claude Code hands to `security -i`.
const SECURITY_LINE_MAX: usize = 4032;

/// Claude Code's account for a new item: `$USER` when it is plain, else a fixed name.
fn default_account() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty() && u.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .unwrap_or_else(|| "claude-code-user".into())
}

/// Escapes a value for a double-quoted `security -i` argument.
fn quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// A JSON file (`.credentials.json` style), written atomically with mode 0600.
#[derive(Debug, Clone)]
pub struct FileSecrets {
    path: PathBuf,
}

impl FileSecrets {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

impl SecretStore for FileSecrets {
    fn read(&self) -> BoxFuture<'_, std::io::Result<Option<String>>> {
        Box::pin(async move {
            match tokio::fs::read_to_string(&self.path).await {
                Ok(s) => Ok(Some(s)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    fn write<'a>(&'a self, value: &'a str) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            use std::os::unix::fs::OpenOptionsExt;
            let tmp = self.path.with_extension("tmp");
            let path = tmp.clone();
            let value = value.to_owned();
            tokio::task::spawn_blocking(move || {
                use std::io::Write;
                let mut f =
                    std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
                f.write_all(value.as_bytes())?;
                f.sync_all()
            })
            .await
            .map_err(std::io::Error::other)??;
            tokio::fs::rename(&tmp, &self.path).await
        })
    }
}

/// Claude Code's "keychain-with-plaintext-fallback" store: reads the
/// primary item, else the plaintext `.credentials.json`; a write goes to the
/// primary (and, when that worked and the primary was empty, the plaintext
/// copy is removed, as Claude Code migrates it), else to the file.
pub struct FallbackSecrets {
    primary: std::sync::Arc<dyn SecretStore>,
    file: FileSecrets,
}

impl std::fmt::Debug for FallbackSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FallbackSecrets").field("file", &self.file.path).finish_non_exhaustive()
    }
}

impl FallbackSecrets {
    pub fn new(primary: std::sync::Arc<dyn SecretStore>, file: impl Into<PathBuf>) -> Self {
        Self { primary, file: FileSecrets::new(file) }
    }
}

impl SecretStore for FallbackSecrets {
    fn read(&self) -> BoxFuture<'_, std::io::Result<Option<String>>> {
        Box::pin(async move {
            if let Some(value) = self.primary.read().await.ok().flatten() {
                return Ok(Some(value));
            }
            self.file.read().await
        })
    }

    fn write<'a>(&'a self, value: &'a str) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            let before = self.primary.read().await.ok().flatten();
            match self.primary.write(value).await {
                Ok(()) => {
                    if before.is_none() {
                        let _ = tokio::fs::remove_file(&self.file.path).await;
                    }
                    Ok(())
                }
                Err(_) => self.file.write(value).await,
            }
        })
    }
}

/// A secret document held in memory.
#[derive(Debug, Default)]
pub struct MemorySecrets(Mutex<Option<String>>);

impl SecretStore for MemorySecrets {
    fn read(&self) -> BoxFuture<'_, std::io::Result<Option<String>>> {
        let value = self.0.lock().unwrap_or_else(PoisonError::into_inner).clone();
        Box::pin(async move { Ok(value) })
    }

    fn write<'a>(&'a self, value: &'a str) -> BoxFuture<'a, std::io::Result<()>> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(value.to_owned());
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_plaintext_file_backs_an_empty_primary() {
        let dir = std::env::temp_dir().join(format!("eos-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join(".credentials.json");
        std::fs::write(&file, r#"{"mcpOAuth":{}}"#).expect("write");
        let primary = std::sync::Arc::new(MemorySecrets::default());
        let store = FallbackSecrets::new(primary.clone(), &file);
        assert_eq!(store.read().await.expect("read").as_deref(), Some(r#"{"mcpOAuth":{}}"#));
        store.write("{}").await.expect("write");
        assert_eq!(primary.read().await.expect("read").as_deref(), Some("{}"));
        assert!(!file.exists(), "migrated into the primary, as Claude Code does");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn store_dir_items_are_hashed_like_claude_code() {
        // Same rule Eos uses (infra/src/platform/keychain.ts): sha256 of the NFC path, first 8 hex chars.
        let k = KeychainSecrets::for_claude_store(Some(Path::new("/tmp/store")));
        let expected = &hex::encode(Sha256::digest(b"/tmp/store"))[..8];
        assert_eq!(k.service(), format!("Claude Code-credentials-{expected}"));
        assert_eq!(KeychainSecrets::for_claude_store(None).service(), "Claude Code-credentials");
    }
}
