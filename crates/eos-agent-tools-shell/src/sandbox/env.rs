//! The environment of a sandboxed command: sandbox-runtime's variables,
//! with per-command proxy credentials on the shared HTTP+SOCKS port.

use std::path::Path;

use base64::Engine as _;

use super::proxy::Lease;

/// The variables sandbox-runtime sets for a sandboxed command (`jpt`), with
/// per-command proxy credentials on the shared HTTP+SOCKS port.
/// `java_unset`: `JAVA_TOOL_OPTIONS` is a `deny` credential, so its inherited
/// value is not carried over.
pub(super) fn command_env(tmpdir: &Path, proxy: Option<(u16, &Lease)>, java_unset: bool) -> Vec<(String, String)> {
    let mut env =
        vec![("SANDBOX_RUNTIME".to_owned(), "1".to_owned()), ("TMPDIR".to_owned(), tmpdir.display().to_string())];
    let Some((port, lease)) = proxy else { return env };
    let mut set = |k: &str, v: String| env.push((k.to_owned(), v));
    let no_proxy = "localhost,127.0.0.1,::1,169.254.0.0/16,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16";
    set("NO_PROXY", no_proxy.into());
    set("no_proxy", no_proxy.into());
    let auth = format!("{}:{}@", lease.user, lease.password);
    let http = format!("http://{auth}127.0.0.1:{port}");
    let socks = format!("socks5h://{auth}127.0.0.1:{port}");
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        set(k, http.clone());
    }
    set("GIT_CONFIG_PARAMETERS", "'http.proxyAuthMethod=basic'".into());
    for k in ["ALL_PROXY", "all_proxy", "GRPC_PROXY", "grpc_proxy"] {
        set(k, http.clone());
    }
    let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", lease.user, lease.password));
    set("SRT_SSH_PROXY_COMMAND", ssh_proxy_command(port, &token));
    set(
        "GIT_SSH_COMMAND",
        "ssh -o ControlMaster=no -o ControlPath=none -o ProxyCommand=\"/bin/sh -c 'eval \\\"\\${SRT_SSH_PROXY_COMMAND:?unset, but GIT_SSH_COMMAND needs it}\\\"' sh '%h' %p\"".into(),
    );
    set("FTP_PROXY", socks.clone());
    set("ftp_proxy", socks);
    set("RSYNC_PROXY", format!("127.0.0.1:{port}"));
    set("DOCKER_HTTP_PROXY", http.clone());
    set("DOCKER_HTTPS_PROXY", http);
    set("CLOUDSDK_PROXY_TYPE", "http".into());
    set("CLOUDSDK_PROXY_ADDRESS", "127.0.0.1".into());
    set("CLOUDSDK_PROXY_PORT", port.to_string());
    set("CLOUDSDK_PROXY_USERNAME", lease.user.clone());
    set("CLOUDSDK_PROXY_PASSWORD", lease.password.clone());
    let java = std::env::var("JAVA_TOOL_OPTIONS").ok().filter(|j| !java_unset && !j.trim().is_empty());
    let flag = "-Djava.net.preferIPv4Stack=true";
    set("JAVA_TOOL_OPTIONS", java.map_or_else(|| flag.to_owned(), |j| format!("{j} {flag}")));
    env
}

/// The variables that make tools trust the TLS-terminating proxy's CA
/// (sandbox-runtime's `jpt`): every one names the trust bundle, whatever the
/// command inherited.
pub(super) fn trust_env(bundle: &Path) -> Vec<(String, String)> {
    const VARS: &[&str] = &[
        "NODE_EXTRA_CA_CERTS",
        "SSL_CERT_FILE",
        "CURL_CA_BUNDLE",
        "REQUESTS_CA_BUNDLE",
        "PIP_CERT",
        "GIT_SSL_CAINFO",
        "AWS_CA_BUNDLE",
        "CARGO_HTTP_CAINFO",
        "DENO_CERT",
        "CLOUDSDK_CORE_CUSTOM_CA_CERTS_FILE",
        "NIX_SSL_CERT_FILE",
    ];
    VARS.iter().map(|k| ((*k).to_owned(), bundle.display().to_string())).collect()
}

/// sandbox-runtime's ssh ProxyCommand (`de`): CONNECT with the command's
/// credentials through `/usr/bin/nc`.
fn ssh_proxy_command(port: u16, token: &str) -> String {
    format!(
        "LC_ALL=C; no() {{ printf \"sandbox proxy: %s\\n\" \"$*\" >&2; exit 1; }}; case $1 in \"\"|*[!A-Za-z0-9._:-]*) no bad host;; \
         *:*) set -- \"[$1]\" \"$2\";; esac; case $2 in \"\"|*[!0-9]*) no bad port;; esac; exec 3<&0; {{ printf \"CONNECT %s \
         HTTP/1.1\\r\\nHost: %s\\r\\nProxy-Authorization: Basic {token}\\r\\n\\r\\n\" \"$1:$2\" \"$1:$2\"; exec /bin/cat <&3; }} | \
         /usr/bin/nc 127.0.0.1 {port} | {{ read -r v why; while read -r v; do case $v in \"\"|?) break;; esac; done; case $why in \
         \"200 \"*) exec /bin/cat;; esac; why=${{why%?}}; no \"$1:$2: ${{why:-no answer}}\"; }} & exec >&-; wait $!"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_proxy_command_matches_sandbox_runtime() {
        let cmd = ssh_proxy_command(7000, "dTpw");
        assert!(cmd.starts_with("LC_ALL=C; no() { printf \"sandbox proxy: %s\\n\" \"$*\" >&2; exit 1; }; case $1 in"));
        assert!(cmd.contains("Proxy-Authorization: Basic dTpw\\r\\n\\r\\n"));
        assert!(cmd.ends_with("| /usr/bin/nc 127.0.0.1 7000 | { read -r v why; while read -r v; do case $v in \"\"|?) break;; esac; done; case $why in \"200 \"*) exec /bin/cat;; esac; why=${why%?}; no \"$1:$2: ${why:-no answer}\"; } & exec >&-; wait $!"));
    }
}
