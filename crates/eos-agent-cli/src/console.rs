//! One owner of the terminal, as Claude Code's single input loop: `chat`'s
//! prompt line and the question dialogs are served by one thread, so a
//! question that arrives while the prompt waits for the user's next line
//! takes the keyboard from it (the prompt is put away with what was typed,
//! and comes back once the dialog closes) instead of both reading keys.
//! Everything the CLI prints goes through [`print`]: held back while a dialog
//! is open and shown once it closes, as in Claude Code, and kept clear of the
//! prompt line while one is shown.

use std::collections::VecDeque;
use std::io::Write as _;
use std::sync::mpsc as std_mpsc;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, queue, terminal};
use tokio::sync::oneshot;
use unicode_width::UnicodeWidthStr as _;

/// Where text goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Out,
    Err,
}

const PROMPT: &str = "> ";

#[derive(Debug, Default)]
struct Screen {
    /// A dialog has the terminal: output waits.
    dialog_open: bool,
    held: Vec<(Stream, String)>,
    /// The prompt line being typed, while one is shown.
    prompt: Option<Prompt>,
}

#[derive(Debug, Default)]
struct Prompt {
    text: String,
    /// Output ended mid-line; the prompt comes back after its next newline.
    hidden: bool,
    /// Rows the drawn prompt takes (0 while not drawn).
    rows: usize,
}

static SCREEN: Mutex<Screen> = Mutex::new(Screen { dialog_open: false, held: Vec::new(), prompt: None });

fn screen() -> std::sync::MutexGuard<'static, Screen> {
    SCREEN.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Prints `text`: now, or once the open dialog closes.
pub(crate) fn print(stream: Stream, text: &str) {
    let mut screen = screen();
    if screen.dialog_open {
        screen.held.push((stream, text.to_owned()));
        return;
    }
    write_out(&mut screen, stream, text);
}

pub(crate) fn println(stream: Stream, text: &str) {
    print(stream, &format!("{text}\n"));
}

fn write_out(screen: &mut Screen, stream: Stream, text: &str) {
    let Some(prompt) = screen.prompt.as_mut() else {
        raw_write(stream, text);
        return;
    };
    // Raw mode while the prompt is up: put it away, write, bring it back.
    let mut err = std::io::stderr();
    erase(&mut err, prompt.rows);
    prompt.rows = 0;
    raw_write(stream, &text.replace('\n', "\r\n"));
    if !text.is_empty() {
        prompt.hidden = !text.ends_with('\n');
    }
    if !prompt.hidden {
        prompt.rows = draw_prompt(&mut err, &prompt.text);
    }
}

/// Whether the last text written ended a line.
static AT_LINE_START: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn raw_write(stream: Stream, text: &str) {
    if let Some(last) = text.chars().last() {
        AT_LINE_START.store(last == '\n', std::sync::atomic::Ordering::Relaxed);
    }
    match stream {
        Stream::Out => {
            let mut out = std::io::stdout();
            let _ = out.write_all(text.as_bytes());
            let _ = out.flush();
        }
        Stream::Err => {
            let mut err = std::io::stderr();
            let _ = err.write_all(text.as_bytes());
            let _ = err.flush();
        }
    }
}

/// Takes back a drawing of `rows` rows that ends where the cursor is.
fn erase(out: &mut impl std::io::Write, rows: usize) {
    if rows == 0 {
        return;
    }
    let _ = queue!(out, crossterm::style::Print("\r"));
    if rows > 1 {
        let _ = queue!(out, cursor::MoveUp(u16::try_from(rows - 1).unwrap_or(u16::MAX)));
    }
    let _ = queue!(out, terminal::Clear(terminal::ClearType::FromCursorDown));
    let _ = out.flush();
}

/// Draws `> text`; returns how many rows it took.
fn draw_prompt(out: &mut impl std::io::Write, text: &str) -> usize {
    let width = usize::from(terminal::size().map_or(80, |(w, _)| w)).max(1);
    let shown = format!("{PROMPT}{text}");
    let rows = shown.split('\n').map(|line| line.width().max(1).div_ceil(width)).sum::<usize>().max(1);
    let _ = out.write_all(shown.replace('\n', "\r\n").as_bytes());
    let _ = out.flush();
    rows
}

/// A dialog takes the terminal: the prompt is put away, output waits.
pub(crate) fn dialog_opens() {
    let mut screen = screen();
    screen.dialog_open = true;
    if let Some(prompt) = screen.prompt.as_mut() {
        erase(&mut std::io::stderr(), prompt.rows);
        prompt.rows = 0;
    } else if !AT_LINE_START.load(std::sync::atomic::Ordering::Relaxed) {
        // Below what was being written, not inside it.
        raw_write(Stream::Err, "\n");
    }
}

/// The dialog is gone: what waited is shown, then the prompt again.
pub(crate) fn dialog_closed() {
    let mut screen = screen();
    screen.dialog_open = false;
    if screen.prompt.is_some() {
        // The dialog left raw mode; the prompt line still needs it.
        let _ = terminal::enable_raw_mode();
        let _ = queue!(std::io::stderr(), event::EnableBracketedPaste);
    }
    let held = std::mem::take(&mut screen.held);
    if held.is_empty()
        && let Some(prompt) = screen.prompt.as_mut()
        && !prompt.hidden
    {
        prompt.rows = draw_prompt(&mut std::io::stderr(), &prompt.text);
    }
    for (stream, text) in held {
        write_out(&mut screen, stream, &text);
    }
}

/// What the owner thread is asked to do.
enum Request {
    Line(oneshot::Sender<Option<String>>),
    /// Runs on the owner thread, which then answers by itself.
    Dialog(Box<dyn FnOnce() + Send>),
}

static OWNER: OnceLock<Mutex<std_mpsc::Sender<Request>>> = OnceLock::new();

fn send(request: Request) {
    let owner = OWNER.get_or_init(|| {
        let (tx, rx) = std_mpsc::channel();
        let _ = std::thread::Builder::new().name("terminal".into()).spawn(move || own(&rx));
        Mutex::new(tx)
    });
    let _ = owner.lock().unwrap_or_else(PoisonError::into_inner).send(request);
}

/// The user's next line from the terminal; `None` at Ctrl-D (or Ctrl-C on
/// an empty line).
pub(crate) async fn read_line() -> Option<String> {
    let (tx, rx) = oneshot::channel();
    send(Request::Line(tx));
    rx.await.ok().flatten()
}

/// Runs `dialog` on the terminal's owner, after any other dialog.
pub(crate) async fn dialog<T: Send + 'static>(dialog: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = oneshot::channel();
    send(Request::Dialog(Box::new(move || {
        dialog_opens();
        let out = dialog();
        dialog_closed();
        let _ = tx.send(out);
    })));
    rx.await.ok()
}

/// The owner thread: dialogs first, then the prompt line, else waiting.
fn own(requests: &std_mpsc::Receiver<Request>) {
    type Pending = Option<(LineEditor, oneshot::Sender<Option<String>>)>;
    fn take(request: Request, line: &mut Pending, dialogs: &mut VecDeque<Box<dyn FnOnce() + Send>>) {
        match request {
            Request::Line(tx) => *line = Some((LineEditor::default(), tx)),
            Request::Dialog(run) => dialogs.push_back(run),
        }
    }
    let mut line: Pending = None;
    let mut dialogs: VecDeque<Box<dyn FnOnce() + Send>> = VecDeque::new();
    loop {
        while let Ok(request) = requests.try_recv() {
            take(request, &mut line, &mut dialogs);
        }
        if let Some(run) = dialogs.pop_front() {
            run();
            continue;
        }
        let Some((editor, _)) = line.as_mut() else {
            match requests.recv() {
                Ok(request) => take(request, &mut line, &mut dialogs),
                Err(_) => return,
            }
            continue;
        };
        if screen().prompt.is_none() {
            start_prompt();
        }
        let key = match event::poll(Duration::from_millis(50)) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) if key.kind != KeyEventKind::Release => Some(LineKey::from(key)),
                Ok(Event::Paste(text)) => Some(LineKey::Paste(text)),
                _ => None,
            },
            Ok(false) => None,
            Err(_) => Some(LineKey::Eof),
        };
        let Some(key) = key else { continue };
        if let Some(done) = editor.key(key) {
            end_prompt();
            if let Some((_, tx)) = line.take() {
                let _ = tx.send(done);
            }
        } else {
            redraw_prompt(&editor.text);
        }
    }
}

fn start_prompt() {
    let _ = terminal::enable_raw_mode();
    let _ = queue!(std::io::stderr(), event::EnableBracketedPaste);
    let mut screen = screen();
    let mut err = std::io::stderr();
    let _ = err.write_all(b"\r\n");
    let rows = draw_prompt(&mut err, "");
    screen.prompt = Some(Prompt { rows, ..Prompt::default() });
}

fn redraw_prompt(text: &str) {
    let mut screen = screen();
    if screen.dialog_open {
        return;
    }
    let Some(prompt) = screen.prompt.as_mut() else { return };
    prompt.text = text.to_owned();
    if prompt.hidden {
        return;
    }
    let mut err = std::io::stderr();
    erase(&mut err, prompt.rows);
    prompt.rows = draw_prompt(&mut err, text);
}

/// The line is in: it stays on screen as typed, the terminal goes back to normal.
fn end_prompt() {
    let mut screen = screen();
    let mut err = std::io::stderr();
    if let Some(prompt) = screen.prompt.take()
        && prompt.hidden
    {
        let _ = err.write_all(b"\r\n");
        draw_prompt(&mut err, &prompt.text);
    }
    let _ = err.write_all(b"\r\n");
    let _ = queue!(err, event::DisableBracketedPaste);
    let _ = err.flush();
    let _ = terminal::disable_raw_mode();
}

/// A key the prompt line reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LineKey {
    Char(char),
    Backspace,
    Enter,
    /// Ctrl-U: the whole line.
    ClearLine,
    /// Ctrl-W: the word before the cursor.
    DeleteWord,
    /// Ctrl-C.
    Interrupt,
    /// Ctrl-D.
    Eof,
    Paste(String),
    Other,
}

impl From<KeyEvent> for LineKey {
    fn from(key: KeyEvent) -> Self {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => Self::Interrupt,
            KeyCode::Char('d') if ctrl => Self::Eof,
            KeyCode::Char('u') if ctrl => Self::ClearLine,
            KeyCode::Char('w') if ctrl => Self::DeleteWord,
            KeyCode::Char(_) if ctrl => Self::Other,
            KeyCode::Char(c) => Self::Char(c),
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Enter => Self::Enter,
            _ => Self::Other,
        }
    }
}

/// The prompt line's text and keys.
#[derive(Debug, Default)]
pub(crate) struct LineEditor {
    pub(crate) text: String,
}

impl LineEditor {
    /// `Some` once the line is done: the text, or `None` to end the chat.
    pub(crate) fn key(&mut self, key: LineKey) -> Option<Option<String>> {
        match key {
            LineKey::Char(c) => self.text.push(c),
            LineKey::Paste(text) => self.text.push_str(&text.replace("\r\n", "\n").replace('\r', "\n")),
            LineKey::Backspace => {
                self.text.pop();
            }
            LineKey::Enter => return Some(Some(std::mem::take(&mut self.text))),
            LineKey::ClearLine => self.text.clear(),
            LineKey::DeleteWord => {
                let kept = self.text.trim_end().rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.text.truncate(kept);
            }
            LineKey::Interrupt if !self.text.is_empty() => self.text.clear(),
            LineKey::Interrupt => return Some(None),
            LineKey::Eof if self.text.is_empty() => return Some(None),
            LineKey::Eof | LineKey::Other => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(keys: Vec<LineKey>) -> (String, Option<Option<String>>) {
        let mut editor = LineEditor::default();
        let done = keys.into_iter().find_map(|k| editor.key(k));
        (editor.text, done)
    }

    #[test]
    fn the_prompt_line_edits() {
        use LineKey::*;
        assert_eq!(typed(vec![Char('h'), Char('i'), Enter]).1, Some(Some("hi".into())));
        assert_eq!(typed(vec![Char('a'), Backspace, Char('b'), Enter]).1, Some(Some("b".into())));
        assert_eq!(typed(vec![Paste("one two\r\nthree".into()), DeleteWord]).0, "one two\n");
        assert_eq!(typed(vec![Char('x'), ClearLine]).0, "");
        assert_eq!(typed(vec![Char('x'), Interrupt]), (String::new(), None), "Ctrl-C clears a line");
        assert_eq!(typed(vec![Interrupt]).1, Some(None), "and ends the chat on an empty one");
        assert_eq!(typed(vec![Char('x'), Eof]).1, None);
        assert_eq!(typed(vec![Eof]).1, Some(None));
    }

    #[test]
    fn output_waits_while_a_dialog_is_open() {
        dialog_opens();
        print(Stream::Err, "later\n");
        assert_eq!(screen().held, [(Stream::Err, "later\n".to_owned())]);
        dialog_closed();
        assert!(screen().held.is_empty() && !screen().dialog_open);
    }
}
