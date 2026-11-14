//! PDF pages rendered to JPEG by poppler's `pdftoppm`, as Claude Code does.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use eos_agent_core::CancellationToken;
use tokio::process::Command;

use super::media::MAX_PDF_PAGES;
use crate::common::{Fail, Res};

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// The rendered pages' files, in page order.
pub(super) async fn render(
    pdf: &Path,
    out_dir: &Path,
    first: u64,
    last: Option<u64>,
    cancel: &CancellationToken,
    timeout: Duration,
) -> Res<Vec<PathBuf>> {
    let Some(bin) = find_on_path("pdftoppm") else {
        return Err(Fail("pdftoppm is not installed. Install poppler-utils (e.g. `brew install poppler` or `apt-get install poppler-utils`) to enable PDF page rendering.".into()));
    };
    let mut cmd = Command::new(bin);
    cmd.args(["-jpeg", "-r", "100", "-f", &first.to_string()]);
    if let Some(last) = last {
        cmd.args(["-l", &last.to_string()]);
    }
    cmd.arg(pdf).arg(out_dir.join("page")).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    cmd.kill_on_drop(true);
    let child = cmd.spawn().map_err(|e| Fail(format!("pdftoppm failed to start: {e}")))?;
    let output = tokio::select! {
        out = child.wait_with_output() => out.map_err(|e| Fail(format!("pdftoppm failed: {e}")))?,
        () = cancel.cancelled() => return Err(Fail("The read was cancelled.".into())),
        () = tokio::time::sleep(timeout) => return Err(Fail("pdftoppm timed out.".into())),
    };
    if !output.status.success() {
        return Err(Fail(explain(&String::from_utf8_lossy(&output.stderr), first, last)));
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(out_dir)
        .map_err(|e| Fail(e.to_string()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "jpg"))
        .collect();
    files.sort();
    Ok(files)
}

fn explain(stderr: &str, first: u64, last: Option<u64>) -> String {
    if stderr.to_ascii_lowercase().contains("password") {
        return "PDF is password-protected. Please provide an unprotected version.".into();
    }
    if let Some(total) = page_count_from_range_error(stderr) {
        if total == 0 {
            return "PDF reports 0 pages (empty page tree). The PDF may be invalid.".into();
        }
        let asked = match last {
            Some(l) if l != first => format!("pages {first}-{l}"),
            _ => format!("page {first}"),
        };
        let pages = if total == 1 { "page" } else { "pages" };
        return format!(
            "Requested {asked} is outside the document (PDF has {total} {pages}). Use a range within 1-{total}, maximum {MAX_PDF_PAGES} pages per request (e.g. pages: \"1-{}\").",
            total.min(MAX_PDF_PAGES)
        );
    }
    let lower = stderr.to_ascii_lowercase();
    if ["damaged", "corrupt", "invalid", "couldn't find trailer", "couldn't read xref"]
        .iter()
        .any(|w| lower.contains(w))
    {
        return "PDF file is corrupted or invalid.".into();
    }
    format!("pdftoppm failed: {}", stderr.trim())
}

/// "Wrong page range given: … last page (N)".
fn page_count_from_range_error(stderr: &str) -> Option<u64> {
    let line = stderr.lines().find(|l| l.contains("Wrong page range given"))?;
    let tail = &line[line.find("last page (")? + "last page (".len()..];
    tail[..tail.find(')')?].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explains_out_of_range() {
        let msg =
            explain("Wrong page range given: the first page (30) can not be after the last page (12).", 30, Some(31));
        assert!(msg.starts_with("Requested pages 30-31 is outside the document (PDF has 12 pages)"), "{msg}");
    }
}
