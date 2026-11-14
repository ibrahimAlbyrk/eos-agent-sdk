//! The line changes an edit made, as the hunks Claude Code attaches to its
//! Edit and Write results (`structuredPatch`, three lines of context).

use serde::Serialize;

const CONTEXT: usize = 3;
/// Past this many cells the changed middle is not aligned line by line: it
/// is shown as removed and added whole, which is still a correct patch.
const MAX_CELLS: usize = 4_000_000;

/// One hunk, in the shape of jsdiff's `structuredPatch`: 1-based starts, and
/// `lines` prefixed with ` `, `-` or `+`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Hunk {
    old_start: usize,
    old_lines: usize,
    new_start: usize,
    new_lines: usize,
    lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Remove,
    Add,
}

/// The hunks that turn `old` into `new`.
pub(crate) fn structured_patch(old: &str, new: &str) -> Vec<Hunk> {
    let (a, b) = (lines(old), lines(new));
    let ops = diff(&a, &b);
    let mut hunks = Vec::new();
    let (mut i, mut ai, mut bi) = (0, 0, 0);
    while i < ops.len() {
        if ops[i] == Op::Keep {
            (i, ai, bi) = (i + 1, ai + 1, bi + 1);
            continue;
        }
        // A hunk runs from `CONTEXT` kept lines before this change to
        // `CONTEXT` after its last one; changes closer than two contexts join.
        let lead = CONTEXT.min(ai).min(bi);
        let (mut end, mut kept) = (i, 0);
        for (j, op) in ops.iter().enumerate().skip(i) {
            if *op == Op::Keep {
                kept += 1;
                if kept > 2 * CONTEXT {
                    break;
                }
            } else {
                (end, kept) = (j + 1, 0);
            }
        }
        let tail = ops[end..].iter().take(CONTEXT).take_while(|op| **op == Op::Keep).count();
        let mut hunk =
            Hunk { old_start: ai - lead + 1, old_lines: 0, new_start: bi - lead + 1, new_lines: 0, lines: Vec::new() };
        let (mut x, mut y) = (ai - lead, bi - lead);
        let before = std::iter::repeat_n(Op::Keep, lead);
        for op in before.chain(ops[i..end + tail].iter().copied()) {
            match op {
                Op::Keep => {
                    hunk.lines.push(format!(" {}", a[x]));
                    (x, y) = (x + 1, y + 1);
                    hunk.old_lines += 1;
                    hunk.new_lines += 1;
                }
                Op::Remove => {
                    hunk.lines.push(format!("-{}", a[x]));
                    x += 1;
                    hunk.old_lines += 1;
                }
                Op::Add => {
                    hunk.lines.push(format!("+{}", b[y]));
                    y += 1;
                    hunk.new_lines += 1;
                }
            }
        }
        // As diff tools do: an empty side starts at the line before.
        if hunk.old_lines == 0 {
            hunk.old_start -= 1;
        }
        if hunk.new_lines == 0 {
            hunk.new_start -= 1;
        }
        hunks.push(hunk);
        (i, ai, bi) = (end + tail, x, y);
    }
    hunks
}

fn lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// Keep/remove/add steps from `a` to `b`: the shared start and end are kept,
/// the middle aligned by its longest common subsequence.
fn diff(a: &[&str], b: &[&str]) -> Vec<Op> {
    let start = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let end = a[start..].iter().rev().zip(b[start..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (ma, mb) = (&a[start..a.len() - end], &b[start..b.len() - end]);
    let mut ops = vec![Op::Keep; start];
    if ma.len().saturating_mul(mb.len()) > MAX_CELLS {
        ops.extend(std::iter::repeat_n(Op::Remove, ma.len()));
        ops.extend(std::iter::repeat_n(Op::Add, mb.len()));
    } else {
        middle(ma, mb, &mut ops);
    }
    ops.extend(std::iter::repeat_n(Op::Keep, end));
    ops
}

fn middle(a: &[&str], b: &[&str], ops: &mut Vec<Op>) {
    let width = b.len() + 1;
    // lcs[i * width + j]: the longest common subsequence of a[i..] and b[j..].
    let mut lcs = vec![0u32; (a.len() + 1) * width];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i * width + j] = if a[i] == b[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            ops.push(Op::Keep);
            (i, j) = (i + 1, j + 1);
        } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
            ops.push(Op::Remove);
            i += 1;
        } else {
            ops.push(Op::Add);
            j += 1;
        }
    }
    ops.extend(std::iter::repeat_n(Op::Remove, a.len() - i));
    ops.extend(std::iter::repeat_n(Op::Add, b.len() - j));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    #[test]
    fn one_changed_line_gets_three_lines_of_context() {
        let old = numbered(10);
        let new = old.replace("line 5\n", "line five\n");
        let hunks = structured_patch(&old, &new);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!((h.old_start, h.old_lines, h.new_start, h.new_lines), (2, 7, 2, 7));
        assert_eq!(
            h.lines,
            [" line 2", " line 3", " line 4", "-line 5", "+line five", " line 6", " line 7", " line 8"]
        );
        let json = serde_json::to_value(h).expect("json");
        assert_eq!(json["oldStart"], 2);
        assert_eq!(json["newLines"], 7);
    }

    #[test]
    fn far_apart_changes_are_separate_hunks_and_near_ones_join() {
        let old = numbered(30);
        let far = old.replace("line 3\n", "x\n").replace("line 25\n", "y\n");
        let starts: Vec<usize> = structured_patch(&old, &far).iter().map(|h| h.old_start).collect();
        assert_eq!(starts, [1, 22]);
        let near = old.replace("line 3\n", "x\n").replace("line 9\n", "y\n");
        let hunks = structured_patch(&old, &near);
        assert_eq!(hunks.len(), 1);
        assert_eq!((hunks[0].old_start, hunks[0].old_lines), (1, 12));
    }

    #[test]
    fn additions_removals_and_no_change() {
        assert!(structured_patch("a\nb\n", "a\nb\n").is_empty());
        let added = structured_patch("", "a\nb\n");
        assert_eq!((added[0].old_start, added[0].old_lines, added[0].new_start, added[0].new_lines), (0, 0, 1, 2));
        assert_eq!(added[0].lines, ["+a", "+b"]);
        let removed = structured_patch("a\nb\nc\n", "a\nc\n");
        assert_eq!(removed[0].lines, [" a", "-b", " c"]);
        assert_eq!((removed[0].old_lines, removed[0].new_lines), (3, 2));
        // Appending after the last line, with no newline at the end.
        let appended = structured_patch("a", "a\nb");
        assert_eq!(appended[0].lines, [" a", "+b"]);
    }
}
