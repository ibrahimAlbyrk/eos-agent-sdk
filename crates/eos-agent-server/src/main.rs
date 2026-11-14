//! `eos-agent-server`: hosts agents for another program. JSON-RPC 2.0, one
//! message per line, over stdio (default) or a unix socket (`--socket`,
//! one session per connection). The protocol is in `docs/protocol.md`.

mod events;
mod host;
mod params;
mod rpc;
mod session;

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;
use tokio::net::UnixListener;

#[derive(Parser)]
#[command(name = "eos-agent-server", version, about = "Hosts Eos agents over JSON-RPC")]
struct Args {
    /// Listen on this unix socket (mode 0600) instead of serving stdio.
    #[arg(long)]
    socket: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match Args::parse().socket {
        None => {
            session::serve(tokio::io::stdin(), tokio::io::stdout()).await;
            Ok(())
        }
        Some(path) => {
            let listener = bind_private(&path)?;
            let served = tokio::select! {
                result = accept(&listener) => result,
                _ = tokio::signal::ctrl_c() => Ok(()),
            };
            let _ = std::fs::remove_file(&path);
            served
        }
    }
}

async fn accept(listener: &UnixListener) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let (read, write) = stream.into_split();
        tokio::spawn(session::serve(read, write));
    }
}

/// Binds inside a fresh 0700 folder, narrows the socket to 0600 and only
/// then moves it into place, so no other user can connect in between.
fn bind_private(path: &Path) -> anyhow::Result<UnixListener> {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let staging = parent.join(format!(".eos-agent-server-{}", std::process::id()));
    std::fs::create_dir(&staging).with_context(|| format!("creating {}", staging.display()))?;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o700))?;
    let inner = staging.join("sock");
    let bound = UnixListener::bind(&inner).context("binding the socket").and_then(|listener| {
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o600))?;
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
            std::fs::remove_file(path)?;
        }
        std::fs::rename(&inner, path).with_context(|| format!("moving the socket to {}", path.display()))?;
        Ok(listener)
    });
    let _ = std::fs::remove_dir(&staging);
    bound
}
