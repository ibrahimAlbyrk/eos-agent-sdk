//! Telling whether files read earlier changed, with one `stat` each.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Modification time and size; `None` = the path did not exist.
type Stamp = Option<(SystemTime, u64)>;

/// The state of a set of paths (files or directories) when they were read.
/// Missing paths count too, so a file created later shows up as a change.
#[derive(Debug, Default)]
pub(crate) struct Stamps(Vec<(PathBuf, Stamp)>);

impl Stamps {
    pub(crate) fn of(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut paths: Vec<PathBuf> = paths.into_iter().collect();
        paths.sort();
        paths.dedup();
        Self(paths.into_iter().map(|p| (stamp(&p), p)).map(|(s, p)| (p, s)).collect())
    }

    pub(crate) fn changed(&self) -> bool {
        self.0.iter().any(|(path, was)| stamp(path) != *was)
    }
}

pub(crate) fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sees_edits_creations_and_removals() {
        let dir = std::env::temp_dir().join(format!("eos-stamps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let (a, b) = (dir.join("a.md"), dir.join("b.md"));
        std::fs::write(&a, "one").expect("write");
        let stamps = Stamps::of([a.clone(), b.clone()]);
        assert!(!stamps.changed());
        std::fs::write(&a, "one two").expect("write");
        assert!(stamps.changed(), "an edit");
        let stamps = Stamps::of([a.clone(), b.clone()]);
        std::fs::write(&b, "new").expect("write");
        assert!(stamps.changed(), "a new file");
        let stamps = Stamps::of([a.clone(), b.clone()]);
        std::fs::remove_file(&a).expect("remove");
        assert!(stamps.changed(), "a removal");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
