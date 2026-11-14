//! What a command line means for its result: which exit codes are failures,
//! and whether it starts with a long `sleep` (refused in the foreground).

use crate::lexer::lex;

/// Leading sleeps of this many seconds or more are refused in the foreground.
const SLEEP_BLOCK_SECS: f64 = 25.0;

/// How a finished command's exit code reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Exit {
    pub is_error: bool,
    /// Why a non-zero code is not a failure (`grep` found nothing, …).
    pub note: Option<&'static str>,
}

/// Judged by the last command of the line, as the shell's exit code is its.
pub(crate) fn interpret_exit(command: &str, code: i32) -> Exit {
    let line = lex(command);
    let last = line.commands.last().map(|c| c.words.as_slice()).unwrap_or_default();
    let name = last.first().map(|w| w.rsplit('/').next().unwrap_or(w)).unwrap_or("");
    let one_means = match name {
        "grep" | "rg" | "egrep" | "fgrep" => Some("No matches found"),
        "find" => Some("Some directories were inaccessible"),
        "diff" => Some("Files differ"),
        "test" | "[" => Some("Condition is false"),
        "git" => match git_subcommand(last) {
            Some("grep") => Some("No matches found"),
            Some("diff") => Some("Files differ"),
            _ => None,
        },
        _ => None,
    };
    match one_means {
        Some(note) => Exit { is_error: code >= 2, note: (code == 1).then_some(note) },
        None => Exit { is_error: code != 0, note: None },
    }
}

fn git_subcommand(words: &[String]) -> Option<&str> {
    let mut rest = words.iter().skip(1).map(String::as_str);
    while let Some(w) = rest.next() {
        match w {
            "-C" | "-c" | "--git-dir" | "--work-tree" => {
                rest.next();
            }
            w if w.starts_with('-') => {}
            w => return Some(w),
        }
    }
    None
}

/// `Some(what)` when the line starts with `sleep N`, N ≥ 25 seconds.
pub(crate) fn blocked_sleep(command: &str) -> Option<String> {
    let line = lex(command);
    let (first, rest) = line.commands.split_first()?;
    let [sleep, secs] = first.words.as_slice() else { return None };
    if sleep != "sleep" || !secs.chars().all(|c| c.is_ascii_digit() || c == '.') || secs.starts_with('.') {
        return None;
    }
    let secs: f64 = secs.parse().ok()?;
    if secs < SLEEP_BLOCK_SECS {
        return None;
    }
    let rest: Vec<String> = rest.iter().map(|c| c.words.join(" ")).collect();
    Some(if rest.is_empty() {
        format!("standalone sleep {secs}")
    } else {
        format!("sleep {secs} followed by: {}", rest.join(" "))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_without_matches_is_not_an_error() {
        assert_eq!(interpret_exit("cat f | grep x", 1), Exit { is_error: false, note: Some("No matches found") });
        assert!(interpret_exit("grep x f", 2).is_error);
        assert!(!interpret_exit("git -C repo diff --quiet", 1).is_error);
        assert!(interpret_exit("git status", 1).is_error);
        assert_eq!(interpret_exit("make", 0), Exit { is_error: false, note: None });
        assert!(interpret_exit("false", 1).is_error);
    }

    #[test]
    fn long_leading_sleeps_are_blocked() {
        assert_eq!(blocked_sleep("sleep 30"), Some("standalone sleep 30".into()));
        assert_eq!(blocked_sleep("sleep 60 && curl x"), Some("sleep 60 followed by: curl x".into()));
        assert_eq!(blocked_sleep("sleep 25.5; ls"), Some("sleep 25.5 followed by: ls".into()));
        assert_eq!(blocked_sleep("sleep 5 && ls"), None);
        assert_eq!(blocked_sleep("ls && sleep 100"), None);
        assert_eq!(blocked_sleep("sleep 1m"), None);
    }
}
