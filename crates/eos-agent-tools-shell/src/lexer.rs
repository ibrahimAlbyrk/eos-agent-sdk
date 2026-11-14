//! A small shell lexer: enough to split a command line into its simple
//! commands and to tell when it uses syntax we do not judge (substitution,
//! redirection, subshells, …). Never used to run anything.

/// One simple command: its words, quotes removed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Simple {
    pub words: Vec<String>,
    /// Some word holds `$…` or a backquote, so its value is unknown.
    pub expands: bool,
}

/// A command line split at `&&`, `||`, `;`, `|` and newlines.
#[derive(Debug, Default)]
pub(crate) struct Line {
    pub commands: Vec<Simple>,
    /// Redirection (other than `2>&1` or to /dev/null), `&`, `(`, `)`, `{`,
    /// `}` or an unterminated quote: syntax this lexer does not model.
    pub opaque: bool,
    /// A redirection to /dev/null was read past (harmless to the disk, but
    /// still a redirection to a file).
    pub to_null: bool,
}

pub(crate) fn lex(input: &str) -> Line {
    let mut lexer = Lexer { chars: input.chars().collect(), at: 0, line: Line::default(), cmd: Simple::default() };
    lexer.run();
    lexer.line
}

struct Lexer {
    chars: Vec<char>,
    at: usize,
    line: Line,
    cmd: Simple,
}

impl Lexer {
    fn peek(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.at + ahead).copied()
    }

    fn run(&mut self) {
        let mut word: Option<String> = None;
        while let Some(c) = self.peek(0) {
            self.at += 1;
            match c {
                ' ' | '\t' => self.end_word(&mut word),
                '\n' | ';' => self.end_command(&mut word),
                '|' | '&' => {
                    let doubled = self.peek(0) == Some(c);
                    if doubled {
                        self.at += 1;
                    } else if c == '&' {
                        self.line.opaque = true; // background job
                    }
                    self.end_command(&mut word);
                }
                '>' | '<' => {
                    if !self.harmless_redirect(c, &mut word) {
                        self.line.opaque = true;
                    }
                }
                '(' | ')' | '{' | '}' => {
                    self.line.opaque = true;
                    word.get_or_insert_default().push(c);
                }
                '#' if word.is_none() => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.at += 1;
                    }
                }
                '\\' => match self.peek(0) {
                    Some('\n') => self.at += 1,
                    Some(next) => {
                        self.at += 1;
                        word.get_or_insert_default().push(next);
                    }
                    None => {}
                },
                '\'' => {
                    let w = word.get_or_insert_default();
                    match self.chars[self.at..].iter().position(|&c| c == '\'') {
                        Some(end) => {
                            w.extend(&self.chars[self.at..self.at + end]);
                            self.at += end + 1;
                        }
                        None => {
                            self.line.opaque = true;
                            self.at = self.chars.len();
                        }
                    }
                }
                '"' => self.double_quoted(word.get_or_insert_default()),
                '$' | '`' => {
                    self.cmd.expands = true;
                    word.get_or_insert_default().push(c);
                }
                c => word.get_or_insert_default().push(c),
            }
        }
        self.end_command(&mut word);
    }

    fn double_quoted(&mut self, word: &mut String) {
        while let Some(c) = self.peek(0) {
            self.at += 1;
            match c {
                '"' => return,
                '\\' if matches!(self.peek(0), Some('$' | '`' | '"' | '\\' | '\n')) => {
                    if let Some(next) = self.peek(0).filter(|&n| n != '\n') {
                        word.push(next);
                    }
                    self.at += 1;
                }
                '$' | '`' => {
                    self.cmd.expands = true;
                    word.push(c);
                }
                c => word.push(c),
            }
        }
        self.line.opaque = true; // unterminated
    }

    /// `2>&1`, `>/dev/null` and `2>/dev/null` change nothing on disk.
    /// Consumes the redirection when it is one of those.
    fn harmless_redirect(&mut self, c: char, word: &mut Option<String>) -> bool {
        if c == '<' {
            return false;
        }
        let rest: String = self.chars[self.at..].iter().take(12).collect();
        let fd_ok = matches!(word.as_deref(), None | Some("1" | "2"));
        if word.as_deref() == Some("2") && rest.starts_with("&1") {
            *word = None;
            self.at += 2;
            return true;
        }
        let target = rest.trim_start_matches(' ');
        if fd_ok && target.starts_with("/dev/null") && target[9..].chars().next().is_none_or(is_boundary) {
            self.line.to_null = true;
            *word = None;
            self.at += rest.len() - target.len() + 9;
            return true;
        }
        false
    }

    fn end_word(&mut self, word: &mut Option<String>) {
        if let Some(w) = word.take() {
            self.cmd.words.push(w);
        }
    }

    fn end_command(&mut self, word: &mut Option<String>) {
        self.end_word(word);
        let cmd = std::mem::take(&mut self.cmd);
        if !cmd.words.is_empty() {
            self.line.commands.push(cmd);
        } else if cmd.expands {
            self.line.opaque = true;
        }
    }
}

fn is_boundary(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | ';' | '|' | '&')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &Line) -> Vec<Vec<&str>> {
        line.commands.iter().map(|c| c.words.iter().map(String::as_str).collect()).collect()
    }

    #[test]
    fn splits_on_operators_and_strips_quotes() {
        let line = lex(r#"git log --format="%h %s" | head -5 && echo 'a b' ; ls\ x"#);
        assert!(!line.opaque);
        assert_eq!(
            words(&line),
            vec![vec!["git", "log", "--format=%h %s"], vec!["head", "-5"], vec!["echo", "a b"], vec!["ls x"]]
        );
    }

    #[test]
    fn flags_expansion_and_redirection() {
        assert!(lex("echo $HOME").commands[0].expands);
        assert!(lex("echo \"$(id)\"").commands[0].expands);
        assert!(!lex("echo '$HOME'").commands[0].expands);
        assert!(lex("echo hi > out").opaque);
        assert!(lex("cat < in").opaque);
        assert!(lex("sleep 1 &").opaque);
        assert!(lex("(cd x)").opaque);
        assert!(lex("echo 'open").opaque);
        let ok = lex("grep x f 2>&1 | wc -l 2>/dev/null");
        assert!(!ok.opaque);
        assert_eq!(words(&ok), vec![vec!["grep", "x", "f"], vec!["wc", "-l"]]);
        assert!(lex("echo hi >/dev/nullx").opaque);
    }

    #[test]
    fn comments_end_at_newline() {
        let line = lex("ls # list it\npwd");
        assert_eq!(words(&line), vec![vec!["ls"], vec!["pwd"]]);
    }
}
