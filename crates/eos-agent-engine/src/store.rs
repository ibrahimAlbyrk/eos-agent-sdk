use std::path::{Path, PathBuf};

use bytes::Bytes;
use eos_agent_core::{BoxFuture, StoredMessage, TranscriptStore};
use eos_agent_types::{Role, Usage};
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio::io::AsyncWriteExt;

/// One JSONL file per agent, one message per line: the message in wire form
/// (`{"role","content"}`), then, as in Claude Code's transcript entries, the
/// response's `usage` on an assistant message, `isMeta` on the host's context
/// and the line's `timestamp` (ISO 8601, UTC). Reading back keeps only `role`
/// and `content`, so the model receives the same bytes as before. Files are opened per append, so
/// thousands of agents hold no open descriptors.
#[derive(Debug, Clone)]
pub struct JsonlStore {
    dir: PathBuf,
}

impl JsonlStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn path(&self, agent_id: &str) -> PathBuf {
        // Ids are ours (UUIDs) or the host's; keep them to one path segment.
        let safe: String =
            agent_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        self.dir.join(format!("{safe}.jsonl"))
    }

    /// The recorded conversation, for resuming an agent.
    pub async fn load(&self, agent_id: &str) -> std::io::Result<Vec<StoredMessage>> {
        read_all(&self.path(agent_id)).await
    }
}

async fn read_all_at(path: PathBuf) -> std::io::Result<Vec<StoredMessage>> {
    read_all(&path).await
}

async fn read_all(path: &Path) -> std::io::Result<Vec<StoredMessage>> {
    let data = match tokio::fs::read(path).await {
        Ok(d) => Bytes::from(d),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut out = Vec::new();
    let mut start = 0;
    for (i, b) in data.iter().enumerate() {
        if *b == b'\n' {
            if i > start {
                out.push(parse_line(data.slice(start..i)).map_err(std::io::Error::other)?);
            }
            start = i + 1;
        }
    }
    // A last line without its newline is a write the process never finished.
    Ok(out)
}

/// One line back into its message: a bare wire message as it is, a line with
/// `usage`/`isMeta`/`timestamp` cut back to `{"role":…,"content":…}` (the
/// content's bytes untouched), its time and meta mark kept beside it.
fn parse_line(line: Bytes) -> Result<StoredMessage, eos_agent_core::MessageError> {
    #[derive(Deserialize)]
    struct Line<'a> {
        role: Role,
        #[serde(borrow)]
        content: &'a RawValue,
        #[serde(default, borrow)]
        timestamp: Option<&'a str>,
        #[serde(default, rename = "isMeta")]
        is_meta: bool,
    }
    let Ok(parsed) = serde_json::from_slice::<Line<'_>>(&line) else {
        return StoredMessage::from_wire(line);
    };
    let at = parsed.timestamp.and_then(parse_timestamp);
    if parsed.timestamp.is_none() && line.ends_with(b"}]}") {
        return StoredMessage::from_wire(line).map(|m| m.with_recorded_ms(at));
    }
    let role = serde_json::to_string(&parsed.role)?;
    let wire = format!(r#"{{"role":{role},"content":{}}}"#, parsed.content.get());
    StoredMessage::from_wire(Bytes::from(wire)).map(|m| m.with_recorded_ms(at).with_meta(parsed.is_meta))
}

/// The line for `message`: its wire bytes with `usage`, `isMeta` and
/// `timestamp` added.
fn entry_line(message: &StoredMessage, usage: Option<&Usage>, now_ms: u64) -> Vec<u8> {
    let json = message.json();
    let mut line = Vec::with_capacity(json.len() + 160);
    line.extend_from_slice(&json[..json.len() - 1]);
    if let Some(usage) = usage
        && let Ok(usage) = serde_json::to_vec(usage)
    {
        line.extend_from_slice(br#","usage":"#);
        line.extend_from_slice(&usage);
    }
    if message.is_meta() {
        line.extend_from_slice(META);
    }
    line.extend_from_slice(format!(r#","timestamp":"{}"}}"#, format_timestamp(now_ms)).as_bytes());
    line.push(b'\n');
    line
}

const META: &[u8] = br#","isMeta":true"#;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `2026-11-08T10:11:12.345Z`, Claude Code's transcript timestamp.
fn format_timestamp(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rest) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(days.cast_signed());
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rest / 3600, rest % 3600 / 60, rest % 60, ms % 1000)
}

/// The inverse of [`format_timestamp`] (fraction optional, `Z` only).
fn parse_timestamp(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<u64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let rest = s.get(19..)?.strip_suffix('Z')?;
    let ms = match rest.strip_prefix('.') {
        None if rest.is_empty() => 0,
        Some(frac) if !frac.is_empty() && frac.bytes().all(|c| c.is_ascii_digit()) => {
            let digits: String = frac.chars().chain("00".chars()).take(3).collect();
            digits.parse::<u64>().ok()?
        }
        _ => return None,
    };
    let days = u64::try_from(days_from_civil(i64::try_from(y).ok()?, mo, d)).ok()?;
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec) * 1000) + ms)
}

// Howard Hinnant's civil-date algorithms.
fn civil_from_days(z: i64) -> (i64, u64, u64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1).cast_unsigned();
    let m = if mp < 10 { mp + 3 } else { mp - 9 }.cast_unsigned();
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: u64, d: u64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = m.cast_signed();
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d.cast_signed() - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl TranscriptStore for JsonlStore {
    fn load<'a>(&'a self, agent_id: &'a str) -> BoxFuture<'a, std::io::Result<Vec<StoredMessage>>> {
        Box::pin(read_all_at(self.path(agent_id)))
    }

    fn append<'a>(
        &'a self,
        agent_id: &'a str,
        message: &'a StoredMessage,
        usage: Option<&'a Usage>,
    ) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            tokio::fs::create_dir_all(&self.dir).await?;
            let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(self.path(agent_id)).await?;
            let line = entry_line(message, usage, now_ms());
            file.write_all(&line).await?;
            file.flush().await
        })
    }

    /// Keeps the old file beside the new one (`<id>.<unix-ms>.jsonl`), so a
    /// summarized conversation's full history is never thrown away.
    fn replace<'a>(&'a self, agent_id: &'a str, messages: &'a [StoredMessage]) -> BoxFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            tokio::fs::create_dir_all(&self.dir).await?;
            let path = self.path(agent_id);
            if tokio::fs::try_exists(&path).await? {
                let stamp =
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
                tokio::fs::rename(&path, path.with_extension(format!("{stamp}.jsonl"))).await?;
            }
            let mut body = Vec::new();
            for m in messages {
                let json = m.json();
                if m.is_meta() {
                    body.extend_from_slice(&json[..json.len() - 1]);
                    body.extend_from_slice(META);
                    body.push(b'}');
                } else {
                    body.extend_from_slice(json);
                }
                body.push(b'\n');
            }
            let tmp = path.with_extension("jsonl.tmp");
            tokio::fs::write(&tmp, body).await?;
            tokio::fs::rename(&tmp, &path).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eos_agent_types::ContentBlock;

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(format_timestamp(0), "1970-01-01T00:00:00.000Z");
        let ms = 1_794_132_672_345; // 2026-11-08
        let text = format_timestamp(ms);
        assert_eq!(text, "2026-11-08T10:11:12.345Z");
        assert_eq!(parse_timestamp(&text), Some(ms));
        assert_eq!(parse_timestamp("2026-11-08T10:11:12Z"), Some(ms - 345));
        assert_eq!(parse_timestamp("2026-11-08T10:11:12.3Z"), Some(ms - 45));
        assert_eq!(
            parse_timestamp("2024-02-29T00:00:00.000Z").map(format_timestamp).as_deref(),
            Some("2024-02-29T00:00:00.000Z")
        );
        assert_eq!(parse_timestamp("yesterday"), None);
    }

    #[test]
    fn usage_and_time_stay_beside_the_wire_bytes() {
        let message = StoredMessage::new(
            Role::Assistant,
            &[ContentBlock::Text { text: "hi \"there\" }]}".into(), citations: None }],
        )
        .expect("message");
        let usage = Usage { input_tokens: Some(3), output_tokens: Some(5), ..Usage::default() };
        let line = entry_line(&message, Some(&usage), 1_794_132_672_345);
        let value: serde_json::Value = serde_json::from_slice(&line).expect("one JSON line");
        assert_eq!(value["usage"]["output_tokens"], 5);
        assert_eq!(value["timestamp"], "2026-11-08T10:11:12.345Z");
        let back = parse_line(Bytes::from(line[..line.len() - 1].to_vec())).expect("parse");
        assert_eq!(back.json(), message.json(), "the model gets the same bytes");
        assert_eq!(back.recorded_ms(), Some(1_794_132_672_345));
        // Lines written before usage and time were kept still load.
        let old = parse_line(message.json().clone()).expect("old line");
        assert_eq!(old.json(), message.json());
        assert_eq!(old.recorded_ms(), None);
    }

    #[tokio::test]
    async fn the_hosts_context_keeps_its_mark() {
        let dir = std::env::temp_dir().join(format!("eos-agent-store-meta-{}", std::process::id()));
        let store = JsonlStore::new(&dir);
        let seed = StoredMessage::user_text("Earlier: SUMMARY").with_meta(true);
        store.replace("a", std::slice::from_ref(&seed)).await.expect("replaced");
        store.append("a", &StoredMessage::user_text("go on"), None).await.expect("appended");
        store.append("a", &seed, None).await.expect("appended");
        let back = store.load("a").await.expect("loads");
        assert_eq!(back.iter().map(StoredMessage::is_meta).collect::<Vec<_>>(), [true, false, true]);
        assert!(back.iter().all(|m| m.json().ends_with(b"}]}")), "the model gets the wire bytes only");
        assert_eq!(back[0].json(), seed.json());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
