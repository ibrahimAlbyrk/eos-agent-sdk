//! Is a command read-only? Conservative: anything not known to be safe is not.
//! Read-only commands may run alongside other concurrency-safe tool calls.

use crate::lexer::{Simple, lex};

pub fn is_read_only(command: &str) -> bool {
    let line = lex(command);
    !line.opaque && !line.commands.is_empty() && line.commands.iter().all(simple_is_read_only)
}

/// Commands that only read, whatever their arguments.
const ANY_ARGS: &[&str] = &[
    "basename",
    "cal",
    "cat",
    "cmp",
    "column",
    "comm",
    "cut",
    "df",
    "diff",
    "dirname",
    "du",
    "echo",
    "egrep",
    "expand",
    "expr",
    "false",
    "fgrep",
    "fmt",
    "fold",
    "free",
    "grep",
    "groups",
    "head",
    "hexdump",
    "id",
    "locale",
    "ls",
    "md5sum",
    "nl",
    "nproc",
    "numfmt",
    "od",
    "paste",
    "pr",
    "readlink",
    "realpath",
    "rev",
    "seq",
    "sha1sum",
    "sha256sum",
    "sha512sum",
    "shasum",
    "sleep",
    "stat",
    "strings",
    "tac",
    "tail",
    "tr",
    "true",
    "tsort",
    "type",
    "uname",
    "unexpand",
    "uptime",
    "wc",
    "which",
];

fn simple_is_read_only(cmd: &Simple) -> bool {
    if cmd.expands {
        return false;
    }
    let words: Vec<&str> = cmd.words.iter().map(String::as_str).collect();
    let Some((&name, args)) = words.split_first() else { return false };
    if ANY_ARGS.contains(&name) {
        return true;
    }
    match name {
        "pwd" | "whoami" => args.is_empty(),
        "cd" => args.len() <= 1,
        "printf" => args.first().is_some_and(|a| !a.starts_with('-')),
        "find" => !args.iter().any(|a| FIND_ACTIONS.contains(a)),
        "rg" => !args.iter().any(|a| a.starts_with("--pre")),
        "sort" => !args.iter().any(|a| a.starts_with("-o") || a.starts_with("--output") || a.starts_with("--compress")),
        // `uniq in out` writes `out`.
        "uniq" => args.iter().filter(|a| !a.starts_with('-')).count() <= 1,
        "jq" => !args.iter().any(|a| JQ_FILE_FLAGS.iter().any(|f| a.starts_with(f))),
        "git" => git_is_read_only(args),
        "gh" => gh_is_read_only(args),
        "docker" => matches!(args.first(), Some(&("inspect" | "logs"))),
        _ => false,
    }
}

const FIND_ACTIONS: &[&str] =
    &["-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls", "-files0-from"];

const JQ_FILE_FLAGS: &[&str] =
    &["-f", "--from-file", "--rawfile", "--slurpfile", "-L", "--library-path", "--run-tests"];

/// Flags that make any git command write files or run programs.
const GIT_DENIED_FLAGS: &[&str] =
    &["--output", "--ext-diff", "--textconv", "--exec", "--upload-pack", "--receive-pack", "--open-files-in-pager"];

const GIT_READ_SUBCOMMANDS: &[&str] = &[
    "status",
    "diff",
    "log",
    "show",
    "blame",
    "shortlog",
    "describe",
    "rev-parse",
    "rev-list",
    "ls-files",
    "ls-tree",
    "cat-file",
    "merge-base",
    "for-each-ref",
    "grep",
    "show-ref",
    "name-rev",
    "ls-remote",
    "check-ignore",
    "diff-tree",
    "diff-files",
    "diff-index",
    "count-objects",
];

fn git_is_read_only(args: &[&str]) -> bool {
    let mut i = 0;
    // Global options that only change where or how output is shown.
    while let Some(&a) = args.get(i) {
        match a {
            "-C" => i += 2,
            "--no-pager" | "-P" | "--no-optional-locks" => i += 1,
            _ => break,
        }
    }
    let Some((&sub, rest)) = args.get(i..).and_then(<[&str]>::split_first) else { return false };
    if rest.iter().any(|a| GIT_DENIED_FLAGS.iter().any(|f| a.starts_with(f)) || (sub == "grep" && a.starts_with("-O")))
    {
        return false;
    }
    if GIT_READ_SUBCOMMANDS.contains(&sub) {
        return true;
    }
    let positional: Vec<&str> = rest.iter().copied().filter(|a| !a.starts_with('-')).collect();
    match sub {
        "branch" => listing(rest, BRANCH_FLAGS, BRANCH_VALUE_FLAGS),
        "tag" => listing(rest, TAG_FLAGS, TAG_VALUE_FLAGS),
        "remote" => match positional.first() {
            None => rest.iter().all(|a| matches!(*a, "-v" | "--verbose")),
            Some(&("show" | "get-url")) => true,
            Some(_) => false,
        },
        "stash" => matches!(positional.first(), Some(&("list" | "show"))),
        "worktree" => positional.first() == Some(&"list"),
        "reflog" => matches!(positional.first(), None | Some(&("show" | "exists"))),
        "config" => {
            let reads = rest.iter().any(|a| matches!(*a, "--get" | "--get-all" | "--get-regexp" | "--list" | "-l"));
            let writes = rest.iter().any(|a| {
                matches!(*a, "--add" | "--replace-all" | "--edit" | "-e")
                    || a.starts_with("--unset")
                    || a.starts_with("--rename-section")
                    || a.starts_with("--remove-section")
            });
            reads && !writes
        }
        _ => false,
    }
}

const BRANCH_FLAGS: &[&str] = &[
    "-l",
    "--list",
    "-a",
    "--all",
    "-r",
    "--remotes",
    "-v",
    "-vv",
    "--verbose",
    "--color",
    "--no-color",
    "--column",
    "--no-column",
    "--no-abbrev",
    "--merged",
    "--no-merged",
    "--show-current",
    "-i",
    "--ignore-case",
];
const BRANCH_VALUE_FLAGS: &[&str] = &["--contains", "--no-contains", "--points-at", "--sort", "--format", "--abbrev"];
const TAG_FLAGS: &[&str] =
    &["-l", "--list", "--column", "--no-column", "-i", "--ignore-case", "--merged", "--no-merged"];
const TAG_VALUE_FLAGS: &[&str] = &["--contains", "--no-contains", "--points-at", "--sort", "--format", "-n"];

/// `git branch` / `git tag` only list: known flags, and names only as flag
/// values, patterns in list mode, or commits after `--merged`.
fn listing(args: &[&str], flags: &[&str], value_flags: &[&str]) -> bool {
    let list_mode = args.iter().any(|a| matches!(*a, "-l" | "--list"));
    let mut i = 0;
    while let Some(&a) = args.get(i) {
        i += 1;
        if let Some((flag, _)) = a.split_once('=') {
            if !value_flags.contains(&flag) && !flags.contains(&flag) {
                return false;
            }
        } else if value_flags.contains(&a) {
            i += 1;
        } else if a.starts_with("-n") && value_flags.contains(&"-n") {
            // `git tag -n5`
        } else if a.starts_with('-') {
            if !flags.contains(&a) {
                return false;
            }
        } else {
            let after_merged = i >= 2 && matches!(args[i - 2], "--merged" | "--no-merged");
            if !list_mode && !after_merged {
                return false;
            }
        }
    }
    true
}

const GH_READ: &[(&str, &[&str])] = &[
    ("pr", &["view", "list", "diff", "checks", "status"]),
    ("issue", &["view", "list", "status"]),
    ("run", &["view", "list"]),
    ("repo", &["view"]),
    ("release", &["view", "list"]),
    ("search", &["repos", "issues", "prs", "code", "commits"]),
    ("workflow", &["view", "list"]),
    ("label", &["list"]),
    ("auth", &["status"]),
];

fn gh_is_read_only(args: &[&str]) -> bool {
    let (Some(group), Some(sub)) = (args.first(), args.get(1)) else { return false };
    let known = GH_READ.iter().any(|(g, subs)| g == group && subs.contains(sub));
    // --web opens a browser.
    known && !args.iter().any(|a| matches!(*a, "--web" | "-w"))
}

#[cfg(test)]
mod tests {
    use super::is_read_only;

    #[test]
    fn accepts_plain_reads() {
        for cmd in [
            "ls -la",
            "cat README.md | head -20",
            "grep -rn 'fn main' src && wc -l src/*.rs",
            "echo hello",
            "pwd",
            "cd src",
            "find . -name '*.rs' -type f",
            "rg --files -g '*.toml'",
            "diff a.txt b.txt 2>&1",
            "ls missing 2>/dev/null",
            "sort -u names.txt",
            "uniq -c counts.txt",
            "jq '.name' package.json",
            "stat -f %m file; du -sh .",
            "sleep 1",
        ] {
            assert!(is_read_only(cmd), "{cmd}");
        }
    }

    #[test]
    fn accepts_git_and_gh_reads() {
        for cmd in [
            "git status",
            "git -C repo --no-pager log --oneline -n 5",
            "git diff --cached --stat",
            "git show HEAD~1:src/lib.rs",
            "git blame -L 10,20 src/lib.rs",
            "git branch -a",
            "git branch --merged main",
            "git branch --list 'feat/*'",
            "git tag -l 'v1.*'",
            "git tag --sort=-creatordate -n5 --list",
            "git remote -v",
            "git remote show origin",
            "git stash list",
            "git config --get user.email",
            "git rev-parse --abbrev-ref HEAD",
            "git worktree list",
            "gh pr view 12 --json title",
            "gh issue list --state open",
            "gh run view 99 --log",
        ] {
            assert!(is_read_only(cmd), "{cmd}");
        }
    }

    #[test]
    fn rejects_writes_and_unknowns() {
        for cmd in [
            "",
            "rm -rf build",
            "touch x",
            "echo hi > out.txt",
            "echo hi >> out.txt",
            "cat a | tee b",
            "ls; rm x",
            "ls && make",
            "cat $(which ls)",
            "echo `id`",
            "echo \"$HOME\"",
            "ls $DIR",
            "FOO=1 ls",
            "(ls)",
            "{ ls; }",
            "sleep 10 &",
            "cat < input",
            "cat <<EOF\nx\nEOF",
            "find . -delete",
            "find . -name x -exec rm {} ;",
            "rg --pre cat x",
            "sort -o out in",
            "uniq in out",
            "jq -f prog.jq data.json",
            "sed -i s/a/b/ f",
            "awk '{print}' f",
            "xargs rm",
            "python3 script.py",
            "/bin/ls",
            "printf -v x hi",
            "cd a b",
            "pwd -L",
            "echo 'unterminated",
        ] {
            assert!(!is_read_only(cmd), "{cmd:?}");
        }
    }

    #[test]
    fn rejects_git_and_gh_writes() {
        for cmd in [
            "git",
            "git commit -m x",
            "git push",
            "git checkout main",
            "git -c core.pager=evil log",
            "git diff --output=patch.diff",
            "git log --ext-diff",
            "git grep -Ovim foo",
            "git branch new-branch",
            "git branch -d old",
            "git branch -m a b",
            "git tag v2.0",
            "git tag -d v1",
            "git remote add origin url",
            "git stash",
            "git stash pop",
            "git config user.email x@y",
            "git config --get a.b --unset a.b",
            "git reflog expire --all",
            "git worktree add ../x",
            "gh pr create",
            "gh pr merge 1",
            "gh pr view 1 --web",
            "gh api repos/x/y",
            "gh",
            "docker run alpine",
        ] {
            assert!(!is_read_only(cmd), "{cmd:?}");
        }
    }
}
