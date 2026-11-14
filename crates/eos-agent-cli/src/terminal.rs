//! `chat` asks the user on the terminal, as Claude Code's interactive
//! terminal does: AskUserQuestion's questions and an MCP server's step-up
//! question in its dialog (see [`crate::dialog`]), run by the terminal's one
//! owner (see [`crate::console`]): arrow keys, Enter, Space to tick,
//! numbers, Tab between questions, `n` for notes on a previewed choice,
//! Ctrl-G for the editor, Ctrl-V for a clipboard image, Esc to cancel, and
//! Claude Code's idle auto-continue when its `askUserQuestionTimeout` setting
//! (or `CLAUDE_AFK_TIMEOUT_MS`) asks for it. Without a terminal on stdin the
//! questions are put as numbered lines on `/dev/tty` instead, each with its
//! options, "Type something." ("Type something" when several may be chosen)
//! and "Chat about this".

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, SetAttribute, SetForegroundColor};
use crossterm::{cursor, event, queue, terminal};
use eos_agent_core::BoxFuture;
use eos_agent_sdk::{AuthPrompter, AuthRequest, PrintPrompter, StepUpAnswer, StepUpRequest};
use eos_agent_tools_session::{Annotation, NoUser, Question, QuestionOption, Refusal, Reply, UserPrompter};
use eos_agent_types::ContentBlock;

use crate::dialog::{Dialog, Key, Line, Outcome, Pasted, Step, Style, Submission, fit};

/// Lines-mode questions, one at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// Where questions are asked.
#[derive(Debug, Clone)]
enum Asker {
    /// Claude Code's dialog, in raw mode on the terminal.
    Dialog,
    /// Numbered lines on this device (or file).
    Lines(PathBuf),
}

impl Default for Asker {
    fn default() -> Self {
        if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
            Self::Dialog
        } else {
            Self::Lines(PathBuf::from("/dev/tty"))
        }
    }
}

impl Asker {
    /// The outcome, or `None` when nobody could be asked.
    async fn ask(&self, questions: &[Question], afk: Afk) -> Option<Outcome> {
        let owned = questions.to_vec();
        match self.clone() {
            Self::Dialog => crate::console::dialog(move || run_dialog(&owned, afk).ok()).await.flatten(),
            Self::Lines(tty) => tokio::task::spawn_blocking(move || {
                let _one = ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner);
                ask_lines(&tty, &owned)
            })
            .await
            .ok()
            .flatten(),
        }
    }
}

/// Claude Code's idle auto-continue: how long a dialog waits for a user who
/// is away (the terminal not focused, no key pressed), and when it starts
/// counting down on screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Afk {
    timeout: Option<Duration>,
    countdown: Duration,
}

/// Claude Code's countdown threshold (`CLAUDE_AFK_COUNTDOWN_MS`'s default).
const COUNTDOWN: Duration = Duration::from_secs(20);

impl Afk {
    /// From Claude Code's `askUserQuestionTimeout` setting (`60s`, `5m`,
    /// `10m`, `never`; off when unset) in the settings files that apply to
    /// `cwd`, and its `CLAUDE_AFK_TIMEOUT_MS` / `CLAUDE_AFK_COUNTDOWN_MS`.
    pub(crate) fn for_dir(cwd: &Path) -> Self {
        let mut docs: Vec<serde_json::Value> = Vec::new();
        let paths = eos_agent_compat::ClaudePaths::from_env();
        if let Some(paths) = &paths {
            docs.extend(eos_agent_compat::user_settings(paths));
        }
        let (shared, local) = eos_agent_compat::project_settings(cwd);
        docs.extend(shared);
        docs.extend(local);
        if let Some(paths) = &paths {
            docs.extend(eos_agent_compat::managed_settings(paths));
        }
        let setting = docs.iter().rev().find_map(|d| d.get("askUserQuestionTimeout")?.as_str().map(str::to_owned));
        let env = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok());
        Self::from_parts(setting.as_deref(), env("CLAUDE_AFK_TIMEOUT_MS"), env("CLAUDE_AFK_COUNTDOWN_MS"))
    }

    fn from_parts(setting: Option<&str>, timeout_ms: Option<u64>, countdown_ms: Option<u64>) -> Self {
        let set = match setting {
            Some("60s") => Some(60_000),
            Some("5m") => Some(300_000),
            Some("10m") => Some(600_000),
            _ => None,
        };
        // On only when the setting asks for it or the variable is set; the
        // variable then wins.
        let timeout = match (set, timeout_ms) {
            (_, Some(ms)) | (Some(ms), None) => Some(Duration::from_millis(ms)),
            (None, None) => None,
        };
        let countdown = Duration::from_millis(countdown_ms.unwrap_or(20_000)).min(timeout.unwrap_or(COUNTDOWN));
        Self { timeout, countdown }
    }
}

#[derive(Debug)]
pub(crate) struct TerminalUser {
    asker: Asker,
    afk: Afk,
}

impl TerminalUser {
    pub(crate) fn new(cwd: &Path) -> Self {
        Self { asker: Asker::default(), afk: Afk::for_dir(cwd) }
    }
}

impl UserPrompter for TerminalUser {
    fn ask<'a>(
        &'a self,
        agent_id: &'a str,
        tool_use_id: &'a str,
        questions: &'a [Question],
    ) -> BoxFuture<'a, Result<Reply, Refusal>> {
        Box::pin(async move {
            match self.asker.ask(questions, self.afk).await {
                Some(Outcome::Submitted(sub)) => Ok(reply(sub)),
                Some(Outcome::Clarify { answers, notes, images }) => Err(Refusal::Said {
                    text: clarify(questions, &answers, &notes),
                    images: images.iter().map(image_block).collect(),
                }),
                Some(Outcome::Cancelled) => Err(Refusal::Cancelled),
                // No terminal to ask on: as a session with nobody to ask.
                None => NoUser.ask(agent_id, tool_use_id, questions).await,
            }
        })
    }
}

fn reply(sub: Submission) -> Reply {
    let annotations = sub.annotations.into_iter().map(|(q, preview, notes)| (q, Annotation { preview, notes }));
    Reply {
        answers: sub.answers.into_iter().collect(),
        annotations: annotations.collect(),
        response: None,
        afk_timeout_ms: sub.afk_ms,
        images: sub.images.iter().map(image_block).collect(),
    }
}

fn image_block(image: &Pasted) -> ContentBlock {
    let data = base64::engine::general_purpose::STANDARD.encode(&image.data);
    let json = serde_json::json!({"type": "image", "source": {"type": "base64", "media_type": image.media_type, "data": data}});
    serde_json::from_value(json).unwrap_or_else(|_| ContentBlock::Text { text: "[image]".into(), citations: None })
}

/// Claude Code's feedback for "Chat about this".
fn clarify(questions: &[Question], answers: &[Option<String>], notes: &[Option<String>]) -> String {
    let asked: Vec<String> = questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let mut lines = vec![format!("- \"{}\"", q.question)];
            lines.push(match answers.get(i).cloned().flatten() {
                Some(a) => format!("  Answer: {a}"),
                None => "  (No answer provided)".to_owned(),
            });
            if let Some(Some(n)) = notes.get(i) {
                lines.push(format!("  User notes: {n}"));
            }
            lines.join("\n")
        })
        .collect();
    format!(
        "The user wants to clarify these questions.\n    This means they may have additional information, context or \
         questions for you.\n    Take their response into account and then reformulate the questions if \
         appropriate.\n    Start by asking them what they would like to clarify.\n\n    Questions asked:\n{}",
        asked.join("\n")
    )
}

/// MCP sign-ins as [`PrintPrompter`] shows them; the step-up question in
/// Claude Code's dialog (header "MCP", its two choices), as Claude Code asks
/// it through AskUserQuestion's view. Anything but "Re-authenticate in
/// browser" declines.
#[derive(Default)]
pub(crate) struct TerminalAuth {
    print: PrintPrompter,
    asker: Asker,
    afk: Afk,
}

impl TerminalAuth {
    pub(crate) fn new(cwd: &Path) -> Self {
        Self { afk: Afk::for_dir(cwd), ..Self::default() }
    }
}

impl AuthPrompter for TerminalAuth {
    fn authorize(&self, request: &AuthRequest) {
        self.print.authorize(request);
    }

    fn ask_step_up<'a>(&'a self, request: &'a StepUpRequest) -> BoxFuture<'a, StepUpAnswer> {
        if matches!(self.asker, Asker::Lines(_)) {
            return self.print.ask_step_up(request);
        }
        Box::pin(async move {
            let options = request
                .choices
                .iter()
                .map(|c| QuestionOption { label: c.label.into(), description: c.description.into(), preview: None })
                .collect();
            let question =
                Question { question: request.question.clone(), header: "MCP".into(), options, multi_select: false };
            let reauthenticate = request.choices.iter().find(|c| c.value == "reauthenticate").map(|c| c.label);
            match self.asker.ask(std::slice::from_ref(&question), self.afk).await {
                Some(Outcome::Submitted(sub))
                    if sub.answers.first().map(|(_, a)| a.as_str()) == reauthenticate && sub.afk_ms.is_none() =>
                {
                    StepUpAnswer::Reauthenticate
                }
                Some(_) => StepUpAnswer::NotNow,
                None => StepUpAnswer::Unanswered,
            }
        })
    }
}

/// Leaves raw mode (and the terminal reports) however the dialog ends.
struct RawMode;

impl RawMode {
    fn on() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        let _ = queue!(std::io::stderr(), event::EnableBracketedPaste, event::EnableFocusChange);
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = queue!(std::io::stderr(), event::DisableBracketedPaste, event::DisableFocusChange);
        let _ = std::io::stderr().flush();
        let _ = terminal::disable_raw_mode();
    }
}

/// Runs the dialog until it closes: keys in, drawing out, the editor and the
/// clipboard on its request, and the idle timer when one is set.
fn run_dialog(questions: &[Question], afk: Afk) -> std::io::Result<Outcome> {
    let mut out = std::io::stderr();
    let raw = RawMode::on()?;
    let editor = Editor::resolve();
    let mut dialog = Dialog::new(questions).with_editor(editor.as_ref().map(Editor::name));
    let mut drawn = draw(&mut out, &dialog, 0)?;
    let opened = Instant::now();
    let (mut since, mut last_key) = (opened, opened);
    let mut focused: Option<bool> = None;
    let outcome = loop {
        if let Some(timeout) = afk.timeout {
            let now = Instant::now();
            if focused == Some(true) {
                since = now;
            }
            let idle = now.duration_since(since.max(last_key));
            if idle >= timeout {
                let ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
                break dialog.submitted(Some(ms));
            }
            let left = (timeout - idle).as_millis().div_ceil(1000);
            let counting = idle >= timeout.saturating_sub(afk.countdown);
            let shown = counting.then(|| u64::try_from(left).unwrap_or(u64::MAX));
            dialog.set_countdown(shown);
            drawn = draw(&mut out, &dialog, drawn)?;
        }
        if !event::poll(Duration::from_millis(if afk.timeout.is_some() { 250 } else { 1000 }))? {
            continue;
        }
        let key = match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => key_of(key),
            Event::Paste(text) => Some(Key::Paste(text)),
            Event::FocusGained => {
                focused = Some(true);
                continue;
            }
            Event::FocusLost => {
                focused = Some(false);
                continue;
            }
            Event::Resize(..) => {
                drawn = draw(&mut out, &dialog, drawn)?;
                continue;
            }
            _ => continue,
        };
        last_key = Instant::now();
        let Some(key) = key else { continue };
        match dialog.key(key) {
            Step::Done(outcome) => break outcome,
            Step::Stay => {}
            Step::Edit(text) => {
                clear(&mut out, drawn)?;
                out.flush()?;
                drawn = 0;
                if let Some(editor) = &editor {
                    let edited = editor.edit(&text);
                    let _ = terminal::enable_raw_mode();
                    let _ = queue!(out, event::EnableBracketedPaste, event::EnableFocusChange);
                    dialog.edited(edited);
                }
            }
            Step::PasteImage => dialog.pasted_image(clipboard_image()),
        }
        drawn = draw(&mut out, &dialog, drawn)?;
    };
    clear(&mut out, drawn)?;
    queue!(out, cursor::Show)?;
    out.flush()?;
    drop(raw);
    Ok(outcome)
}

fn key_of(key: KeyEvent) -> Option<Key> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    Some(match key.code {
        KeyCode::Char('c' | 'd') if ctrl => Key::Esc,
        KeyCode::Char('p') if ctrl => Key::Up,
        KeyCode::Char('n') if ctrl => Key::Down,
        KeyCode::Char('g') if ctrl => Key::CtrlG,
        KeyCode::Char('v') if ctrl => Key::CtrlV,
        KeyCode::Char(_) if ctrl => return None,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        _ => return None,
    })
}

/// Draws the dialog over the `drawn` lines of the last drawing; returns how
/// many it drew.
fn draw(out: &mut impl std::io::Write, dialog: &Dialog, drawn: usize) -> std::io::Result<usize> {
    let (width, height) = terminal::size().map_or((80, 24), |(w, h)| (usize::from(w), usize::from(h)));
    let width = width.saturating_sub(1).max(10);
    clear(out, drawn)?;
    queue!(out, cursor::Hide)?;
    let lines = dialog.lines(width, height);
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            queue!(out, Print("\r\n"))?;
        }
        print_line(out, &fit(line, width))?;
    }
    out.flush()?;
    Ok(lines.len())
}

/// Takes back the `drawn` lines of the last drawing.
fn clear(out: &mut impl std::io::Write, drawn: usize) -> std::io::Result<()> {
    queue!(out, Print("\r"))?;
    if drawn > 1 {
        queue!(out, cursor::MoveUp(u16::try_from(drawn - 1).unwrap_or(u16::MAX)))?;
    }
    queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown))
}

fn print_line(out: &mut impl std::io::Write, line: &Line) -> std::io::Result<()> {
    for (text, style) in line {
        let reset = SetAttribute(Attribute::Reset);
        match style {
            Style::Plain => queue!(out, Print(text))?,
            Style::Dim => queue!(out, SetAttribute(Attribute::Dim), Print(text), reset)?,
            Style::DimItalic => {
                queue!(out, SetAttribute(Attribute::Dim), SetAttribute(Attribute::Italic), Print(text), reset)?;
            }
            Style::Bold => queue!(out, SetAttribute(Attribute::Bold), Print(text), reset)?,
            Style::Accent => queue!(out, SetForegroundColor(Color::Blue), Print(text), reset)?,
            Style::Success => queue!(out, SetForegroundColor(Color::Green), Print(text), reset)?,
            Style::Warning => queue!(out, SetForegroundColor(Color::Yellow), Print(text), reset)?,
            Style::Current | Style::Inverse => queue!(out, SetAttribute(Attribute::Reverse), Print(text), reset)?,
        }
    }
    Ok(())
}

/// The editor Ctrl-G opens: `$VISUAL`, else `$EDITOR`, else the first of
/// `code`, `vi` and `nano` on the `PATH` (Claude Code's choice).
#[derive(Debug, Clone)]
struct Editor {
    command: String,
}

/// Editors with a window of their own: the terminal stays as it is.
const WINDOWED: [&str; 9] = ["code", "cursor", "windsurf", "codium", "subl", "atom", "gedit", "notepad++", "notepad"];

impl Editor {
    fn resolve() -> Option<Self> {
        let set = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        let command = set("VISUAL").or_else(|| set("EDITOR")).or_else(|| {
            let path = std::env::var_os("PATH")?;
            ["code", "vi", "nano"]
                .into_iter()
                .find(|bin| std::env::split_paths(&path).any(|dir| dir.join(bin).is_file()))
                .map(str::to_owned)
        })?;
        Some(Self { command })
    }

    /// The program's name: its first word that is not a flag.
    fn program(&self) -> String {
        let word = self.command.split_whitespace().find(|w| !w.starts_with('-')).unwrap_or(&self.command);
        Path::new(word).file_name().map_or_else(|| word.to_owned(), |n| n.to_string_lossy().into_owned())
    }

    /// As Claude Code names it in "ctrl+g to edit in …".
    fn name(&self) -> String {
        let program = self.program();
        let known = [
            ("code", "VS Code"),
            ("cursor", "Cursor"),
            ("windsurf", "Devin Desktop"),
            ("antigravity", "Antigravity"),
            ("vi", "Vim"),
            ("vim", "Vim"),
            ("nano", "nano"),
            ("notepad", "Notepad"),
            ("emacs", "Emacs"),
            ("subl", "Sublime Text"),
            ("atom", "Atom"),
        ];
        let key = |s: &str| s.trim().to_lowercase();
        known
            .iter()
            .find(|(k, _)| key(&self.command) == *k || key(&program) == *k)
            .map_or(program.clone(), |(_, name)| (*name).to_owned())
    }

    /// `text` edited in a temporary file; `None` when the editor failed.
    fn edit(&self, text: &str) -> Option<String> {
        let file = std::env::temp_dir().join(format!("claude-prompt-{}.md", uuid::Uuid::new_v4()));
        std::fs::write(&file, text).ok()?;
        let command = match self.command.as_str() {
            "code" => "code -w",
            "subl" => "subl --wait",
            other => other,
        };
        let mut words = command.split_whitespace();
        let program = words.next()?;
        let windowed = WINDOWED.iter().any(|w| self.program().contains(w));
        let _ = terminal::disable_raw_mode();
        if !windowed {
            let _ = queue!(std::io::stderr(), terminal::EnterAlternateScreen, cursor::Show);
            let _ = std::io::stderr().flush();
        }
        let status = std::process::Command::new(program).args(words).arg(&file).status();
        if !windowed {
            let _ = queue!(std::io::stderr(), terminal::LeaveAlternateScreen);
            let _ = std::io::stderr().flush();
        }
        let edited = match status {
            Ok(status) if status.success() => std::fs::read_to_string(&file).ok(),
            _ => None,
        };
        let _ = std::fs::remove_file(&file);
        edited.map(|t| without_final_newline(&t))
    }
}

/// One final newline goes, as Claude Code trims what the editor saved.
fn without_final_newline(text: &str) -> String {
    let newline = if text.ends_with("\r\n") { "\r\n" } else { "\n" };
    if text.ends_with(newline) && !text.ends_with(&format!("{newline}{newline}")) {
        text[..text.len() - newline.len()].to_owned()
    } else {
        text.to_owned()
    }
}

/// The clipboard's image, as Claude Code reads it on macOS (`osascript`,
/// PNG).
fn clipboard_image() -> Result<Pasted, String> {
    const NONE: &str = "No image found in clipboard. Use ctrl+v to paste images.";
    if !cfg!(target_os = "macos") {
        return Err(NONE.into());
    }
    let script = |args: &[&str]| std::process::Command::new("osascript").args(args).output();
    let has = script(&["-e", "the clipboard as \u{ab}class PNGf\u{bb}"]);
    if !has.is_ok_and(|o| o.status.success()) {
        return Err(NONE.into());
    }
    let file = std::env::temp_dir().join(format!("claude_cli_latest_screenshot-{}.png", uuid::Uuid::new_v4()));
    let path = file.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
    let open = format!("set fp to open for access POSIX file \"{path}\" with write permission");
    let saved = script(&[
        "-e",
        "set png_data to (the clipboard as \u{ab}class PNGf\u{bb})",
        "-e",
        &open,
        "-e",
        "write png_data to fp",
        "-e",
        "close access fp",
    ]);
    let data = saved.ok().filter(|o| o.status.success()).and_then(|_| std::fs::read(&file).ok());
    let _ = std::fs::remove_file(&file);
    let data = data.filter(|d| !d.is_empty()).ok_or_else(|| "Couldn't read an image from the clipboard".to_owned())?;
    if data.len().div_ceil(3) * 4 > MAX_IMAGE_BASE64 {
        return Err("The image is too large to attach (at most 5 MB once encoded).".into());
    }
    Ok(Pasted { media_type: "image/png".into(), data })
}

/// What an image may weigh once base64-encoded (the API's limit).
const MAX_IMAGE_BASE64: usize = 5 * 1024 * 1024;

/// Claude Code's free-text choice under a question's options.
fn own_answer(question: &Question) -> &'static str {
    if question.multi_select { "Type something" } else { "Type something." }
}

/// What a typed line chose.
#[derive(Debug, PartialEq, Eq)]
enum Pick {
    Answer(String),
    /// The free-text choice: the answer comes on the next line.
    Own,
    /// "Chat about this".
    Chat,
}

fn ask_lines(tty: &std::path::Path, questions: &[Question]) -> Option<Outcome> {
    let mut out = std::fs::OpenOptions::new().append(true).open(tty).ok()?;
    let mut input = std::io::BufReader::new(std::fs::File::open(tty).ok()?);
    let mut answers: Vec<Option<String>> = Vec::new();
    for question in questions {
        out.write_all(shown(question).as_bytes()).and_then(|()| out.flush()).ok()?;
        let mut line = String::new();
        if input.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let answer = match picked(question, &line) {
            Some(Pick::Answer(answer)) => Some(answer),
            Some(Pick::Own) => {
                out.write_all(b"Your answer: ").and_then(|()| out.flush()).ok()?;
                line.clear();
                input.read_line(&mut line).ok()?;
                Some(line.trim().to_owned()).filter(|a| !a.is_empty())
            }
            Some(Pick::Chat) => {
                answers.resize(questions.len(), None);
                let notes = vec![None; questions.len()];
                return Some(Outcome::Clarify { answers, notes, images: Vec::new() });
            }
            None => None,
        };
        answers.push(answer);
    }
    let answered = questions.iter().zip(answers).filter_map(|(q, a)| Some((q.question.clone(), a?)));
    Some(Outcome::Submitted(Submission { answers: answered.collect(), ..Submission::default() }))
}

/// The question as the numbered lines show it.
fn shown(question: &Question) -> String {
    let mut text = format!("\n[{}] {}\n", question.header, question.question);
    for (i, option) in question.options.iter().enumerate() {
        text += &format!("  {}. {} — {}\n", i + 1, option.label, option.description);
    }
    let own = question.options.len() + 1;
    text += &format!("  {own}. {}\n  {}. Chat about this\n", own_answer(question), own + 1);
    text += if question.multi_select { "Choose one or more (e.g. 1,3): " } else { "Choose one: " };
    text
}

/// What a typed line answers: the options its numbers or labels name (joined
/// with ", " for a multi-select question), the free-text choice or "Chat
/// about this" for their numbers, any other text as typed, nothing for an
/// empty line.
fn picked(question: &Question, line: &str) -> Option<Pick> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if line == (question.options.len() + 1).to_string() {
        return Some(Pick::Own);
    }
    if line == (question.options.len() + 2).to_string() {
        return Some(Pick::Chat);
    }
    let label = |part: &str| -> Option<&str> {
        let part = part.trim();
        question
            .options
            .iter()
            .enumerate()
            .find(|(i, o)| part == (i + 1).to_string() || part.eq_ignore_ascii_case(&o.label))
            .map(|(_, o)| o.label.as_str())
    };
    if let Some(one) = label(line) {
        return Some(Pick::Answer(one.to_owned()));
    }
    if question.multi_select
        && let Some(labels) = line.split(',').map(label).collect::<Option<Vec<&str>>>()
    {
        return Some(Pick::Answer(labels.join(", ")));
    }
    Some(Pick::Answer(line.to_owned()))
}

/// For tests: a dialog-free asker on this file.
#[cfg(test)]
impl TerminalUser {
    fn on(tty: impl Into<PathBuf>) -> Self {
        Self { asker: Asker::Lines(tty.into()), afk: Afk::default() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question(multi_select: bool) -> Question {
        let option =
            |label: &str| QuestionOption { label: label.into(), description: format!("{label}!"), preview: None };
        Question {
            question: "Which?".into(),
            header: "Pick".into(),
            options: vec![option("Red"), option("Blue")],
            multi_select,
        }
    }

    #[test]
    fn a_line_picks_options_or_answers_in_its_own_words() {
        let answer = |text: &str| Some(Pick::Answer(text.into()));
        let one = question(false);
        assert_eq!(picked(&one, "2\n"), answer("Blue"));
        assert_eq!(picked(&one, "red"), answer("Red"));
        assert_eq!(picked(&one, "3"), Some(Pick::Own));
        assert_eq!(picked(&one, "4"), Some(Pick::Chat));
        assert_eq!(picked(&one, "green, please"), answer("green, please"));
        assert_eq!(picked(&one, "  "), None);
        assert!(shown(&one).ends_with("  3. Type something.\n  4. Chat about this\nChoose one: "));
        let many = question(true);
        assert_eq!(picked(&many, "1, 2"), answer("Red, Blue"));
        assert_eq!(picked(&many, "1,3"), answer("1,3"));
        assert!(shown(&many).ends_with(
            "  1. Red — Red!\n  2. Blue — Blue!\n  3. Type something\n  4. Chat about this\nChoose one or more (e.g. \
             1,3): "
        ));
    }

    #[tokio::test]
    async fn questions_are_asked_on_the_terminal() {
        let tty = std::env::temp_dir().join(format!("eos-cli-ask-tty-{}", std::process::id()));
        std::fs::write(&tty, "2\n3\nmy own\n").expect("tty");
        let user = TerminalUser::on(&tty);
        let mut second = question(false);
        second.question = "And?".into();
        let reply = user.ask("a1", "t1", &[question(false), second.clone()]).await.expect("reply");
        assert_eq!(reply.answers["Which?"], "Blue");
        assert_eq!(reply.answers["And?"], "my own");
        let shown = std::fs::read_to_string(&tty).expect("read");
        assert!(shown.contains("[Pick] Which?\n  1. Red — Red!"), "{shown}");
        assert!(shown.ends_with("Your answer: "), "{shown}");

        // "Chat about this": Claude Code's rejection with what was asked.
        std::fs::write(&tty, "1\n4\n").expect("tty");
        let Err(Refusal::Said { text, .. }) = user.ask("a1", "t1", &[question(false), second]).await else {
            panic!("clarify")
        };
        assert_eq!(
            text,
            "The user wants to clarify these questions.\n    This means they may have additional information, \
             context or questions for you.\n    Take their response into account and then reformulate the questions \
             if appropriate.\n    Start by asking them what they would like to clarify.\n\n    Questions asked:\n- \
             \"Which?\"\n  Answer: Red\n- \"And?\"\n  (No answer provided)"
        );
        let _ = std::fs::remove_file(&tty);
        let nowhere = TerminalUser::on("/nonexistent/tty");
        let Err(Refusal::Unanswered(err)) = nowhere.ask("a1", "t1", &[question(false)]).await else { panic!("nobody") };
        assert!(err.starts_with("No user is available to answer questions"), "{err}");
    }

    #[test]
    fn idle_auto_continue_is_off_unless_asked_for() {
        let off = Afk::from_parts(None, None, None);
        assert_eq!(off.timeout, None);
        assert_eq!(Afk::from_parts(Some("never"), None, None).timeout, None);
        let five = Afk::from_parts(Some("5m"), None, None);
        assert_eq!((five.timeout, five.countdown), (Some(Duration::from_secs(300)), Duration::from_secs(20)));
        let env = Afk::from_parts(None, Some(3000), None);
        assert_eq!((env.timeout, env.countdown), (Some(Duration::from_secs(3)), Duration::from_secs(3)));
        let both = Afk::from_parts(Some("60s"), Some(9000), Some(4000));
        assert_eq!((both.timeout, both.countdown), (Some(Duration::from_secs(9)), Duration::from_secs(4)));
    }

    #[test]
    fn clarifying_carries_the_notes() {
        let text = clarify(&[question(false)], &[Some("Red".into())], &[Some("smaller".into())]);
        assert!(text.ends_with("Questions asked:\n- \"Which?\"\n  Answer: Red\n  User notes: smaller"), "{text}");
        let sub = Submission {
            answers: vec![("Which?".into(), "Red".into())],
            annotations: vec![("Which?".into(), Some("<p>".into()), Some("smaller".into()))],
            images: vec![Pasted { media_type: "image/png".into(), data: vec![0xff] }],
            afk_ms: Some(60_000),
        };
        let reply = reply(sub);
        assert_eq!(reply.annotations["Which?"].notes.as_deref(), Some("smaller"));
        assert_eq!(reply.afk_timeout_ms, Some(60_000));
        let image = serde_json::to_value(&reply.images[0]).expect("json");
        assert_eq!(image["source"]["data"], "/w==");
        assert_eq!(without_final_newline("a\n"), "a");
        assert_eq!(without_final_newline("a\n\n"), "a\n\n");
    }
}
