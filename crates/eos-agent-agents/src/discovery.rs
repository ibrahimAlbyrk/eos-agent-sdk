//! Finding agent files on disk.

use std::path::{Path, PathBuf};

use crate::definition::{AgentDefinition, Source};

/// Definitions found under some roots, and the files that were rejected
/// (path, reason) for the host to log.
#[derive(Debug, Default)]
pub struct Discovered {
    pub definitions: Vec<AgentDefinition>,
    pub errors: Vec<(PathBuf, String)>,
}

/// The agent directories Claude Code reads for a session: the user's
/// `~/.claude/agents`, then `.claude/agents` in the working directory and each
/// parent up to the repository root (stopping before `home`), farthest first
/// so nearer ones win. Pure path arithmetic; missing directories are fine.
pub fn agent_roots(home: &Path, cwd: &Path, repo_root: Option<&Path>) -> Vec<(Source, PathBuf)> {
    let mut project = Vec::new();
    for dir in cwd.ancestors() {
        if dir == home {
            break;
        }
        project.push(dir.join(".claude").join("agents"));
        if repo_root == Some(dir) {
            break;
        }
    }
    let mut roots = vec![(Source::User, home.join(".claude").join("agents"))];
    roots.extend(project.into_iter().rev().map(|dir| (Source::Project, dir)));
    roots
}

/// Reads every `*.md` file under each root (recursively). Feed the result to
/// [`AgentDefinitions::new`](crate::AgentDefinitions::new) together with the
/// built-ins; roots are listed in increasing precedence.
pub fn discover(roots: &[(Source, PathBuf)]) -> Discovered {
    let mut found = Discovered::default();
    for (source, root) in roots {
        let mut files = Vec::new();
        markdown_files(root, &mut files);
        files.sort();
        for path in files {
            let text = match std::fs::read_to_string(&path) {
                Ok(text) => text,
                Err(e) => {
                    found.errors.push((path, format!("could not read: {e}")));
                    continue;
                }
            };
            match AgentDefinition::parse(&text, *source, Some(path.clone())) {
                Ok(Some(def)) => found.definitions.push(def),
                Ok(None) => {}
                Err(reason) => found.errors.push((path, reason)),
            }
        }
    }
    found
}

fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        // file_type() does not follow symlinks; metadata() does, so linked agent files count.
        let Ok(meta) = std::fs::metadata(&path) else { continue };
        if meta.is_dir() {
            if entry.file_type().is_ok_and(|t| !t.is_symlink()) {
                markdown_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_walk_up_to_the_repository() {
        let roots = agent_roots(
            Path::new("/Users/me"),
            Path::new("/Users/me/code/repo/pkg"),
            Some(Path::new("/Users/me/code/repo")),
        );
        let paths: Vec<(Source, &str)> = roots.iter().map(|(s, p)| (*s, p.to_str().unwrap_or(""))).collect();
        assert_eq!(
            paths,
            [
                (Source::User, "/Users/me/.claude/agents"),
                (Source::Project, "/Users/me/code/repo/.claude/agents"),
                (Source::Project, "/Users/me/code/repo/pkg/.claude/agents"),
            ]
        );
        let no_repo = agent_roots(Path::new("/Users/me"), Path::new("/Users/me/a/b"), None);
        assert_eq!(no_repo.len(), 3);
    }
}
