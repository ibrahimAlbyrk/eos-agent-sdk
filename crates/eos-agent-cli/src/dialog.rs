//! Claude Code's question dialog (its AskUserQuestion permission view), as
//! a terminal-free state machine: keys in, lines out. A tab per question
//! (`← ☐ Header ✔ Submit →`), the question, its options under a `❯`
//! pointer (checkboxes when several may be chosen), a last "Type
//! something." row for an answer of one's own, "Chat about this" below a
//! rule, and a review tab ("Review your answers") before submitting. A
//! single question with one choice submits on the choice. A single-choice
//! question whose options carry previews is shown side by side instead: the
//! options on the left, the focused one's preview in a box on the right, and
//! notes on the choice (`n`). Opening the editor (Ctrl-G) and reading the
//! clipboard (Ctrl-V) are asked of the caller as [`Step`]s.

use eos_agent_tools_session::Question;
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

/// Claude Code's figures.
const POINTER: &str = "\u{276f}";
const TICK: &str = "\u{2714}";
const BOX_ON: &str = "\u{2612}";
const BOX_OFF: &str = "\u{2610}";
const ARROW_RIGHT: &str = "\u{2192}";
const WARNING: &str = "\u{26a0}";
/// What a choice made with notes alone answers (Claude Code's `XEn`).
const NOTES_ONLY: &str = "(notes only)";
/// Previews longer than this are withheld (Claude Code's `_ft`).
const PREVIEW_MAX: usize = 2000;
const WITHHELD: &str = "(preview cannot be shown in full \u{2014} compare the option labels and descriptions instead)";
/// The width of the options column beside the preview, and the gap after it.
const OPTIONS_WIDTH: usize = 30;
const GAP: usize = 4;
/// A preview box is at least this wide (Claude Code's `Wgt`).
const PREVIEW_MIN_WIDTH: usize = 40;

/// A key the dialog reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Key {
    Up,
    Down,
    Left,
    Right,
    Tab,
    BackTab,
    Enter,
    Esc,
    Backspace,
    Delete,
    Char(char),
    /// Claude Code's `chat:externalEditor`.
    CtrlG,
    /// Claude Code's `chat:imagePaste`.
    CtrlV,
    /// A bracketed paste (empty when the clipboard holds an image, on macOS).
    Paste(String),
}

/// An image pasted into an answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Pasted {
    pub(crate) media_type: String,
    pub(crate) data: Vec<u8>,
}

/// What the user submitted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Submission {
    /// The answers, by question text (several choices joined with ", ").
    pub(crate) answers: Vec<(String, String)>,
    /// Question text, the chosen option's preview, the notes.
    pub(crate) annotations: Vec<(String, Option<String>, Option<String>)>,
    pub(crate) images: Vec<Pasted>,
    /// Submitted by itself after this long without the user.
    pub(crate) afk_ms: Option<u64>,
}

/// How the dialog closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Submitted(Submission),
    /// "Chat about this": the answers and notes so far, by question.
    Clarify {
        answers: Vec<Option<String>>,
        notes: Vec<Option<String>>,
        images: Vec<Pasted>,
    },
    /// Esc, Ctrl-C or Cancel on the review.
    Cancelled,
}

/// What a key leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    Stay,
    Done(Outcome),
    /// Open the editor on this text; hand the result to [`Dialog::edited`].
    Edit(String),
    /// Read an image from the clipboard; hand it to [`Dialog::pasted_image`].
    PasteImage,
}

/// How a piece of a line is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Style {
    Plain,
    Dim,
    Bold,
    /// Claude Code's `suggestion` colour (the focused row).
    Accent,
    /// Claude Code's `success` colour (chosen).
    Success,
    Warning,
    /// The current tab.
    Current,
    /// A selected image chip.
    Inverse,
    /// Notes not being edited.
    DimItalic,
}

pub(crate) type Line = Vec<(String, Style)>;

/// A preview as Claude Code shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Preview {
    Full(String),
    Withheld,
}

fn preview_of(raw: Option<&str>) -> Option<Preview> {
    let raw = raw?;
    if raw.trim().is_empty() {
        return None;
    }
    let controls = raw.chars().any(|c| c.is_control() && c != '\n' && c != '\t');
    if raw.chars().count() > PREVIEW_MAX || controls {
        return Some(Preview::Withheld);
    }
    Some(Preview::Full(raw.to_owned()))
}

#[derive(Debug, Default, Clone)]
struct State {
    focus: usize,
    checked: Vec<bool>,
    other: String,
    other_checked: bool,
    /// The single choice made, to tick it when the question is shown again.
    chosen: Option<String>,
    /// Notes on a previewed choice, and whether they are being written.
    notes: String,
    notes_mode: bool,
    /// The previews' "Chat about this" row has the focus.
    chat_focus: bool,
    /// The focus moved since the question was shown (Enter after notes then
    /// takes the focused option).
    moved: bool,
    images: Vec<(usize, Pasted)>,
    /// The image chip selected (attachments mode).
    image_select: Option<usize>,
}

#[derive(Debug)]
pub(crate) struct Dialog {
    questions: Vec<Question>,
    /// The current tab: a question, or the review tab past the last one.
    tab: usize,
    states: Vec<State>,
    answers: Vec<Option<String>>,
    /// The review tab's focus: 0 "Submit answers", 1 "Cancel".
    review: usize,
    /// The editor's name for the Ctrl-G hint (none: no hint, no editor).
    editor: Option<String>,
    /// A one-off notice under the dialog ("No image found in clipboard…").
    notice: Option<String>,
    /// Seconds left before the dialog answers by itself, while counting down.
    countdown: Option<u64>,
}

impl Dialog {
    pub(crate) fn new(questions: &[Question]) -> Self {
        let states = questions.iter().map(|q| State { checked: vec![false; q.options.len()], ..State::default() });
        Self {
            questions: questions.to_vec(),
            tab: 0,
            states: states.collect(),
            answers: vec![None; questions.len()],
            review: 0,
            editor: None,
            notice: None,
            countdown: None,
        }
    }

    /// The editor Ctrl-G opens, by its display name.
    pub(crate) fn with_editor(mut self, name: Option<String>) -> Self {
        self.editor = name;
        self
    }

    pub(crate) fn set_countdown(&mut self, seconds: Option<u64>) {
        self.countdown = seconds;
    }

    /// One question with one choice: no Submit tab, the choice submits
    /// (Claude Code's `hideSubmitTab`).
    fn single(&self) -> bool {
        self.questions.len() == 1 && !self.questions[0].multi_select
    }

    fn last_tab(&self) -> usize {
        if self.single() { 0 } else { self.questions.len() }
    }

    /// Single choice with a preview on some option: the side-by-side view.
    fn previewed(question: &Question) -> bool {
        !question.multi_select && question.options.iter().any(|o| preview_of(o.preview.as_deref()).is_some())
    }

    fn typing(&self) -> bool {
        self.questions
            .get(self.tab)
            .is_some_and(|q| !Self::previewed(q) && self.states[self.tab].focus == q.options.len())
    }

    /// The question rows: options, "Type something.", the Submit/Next
    /// button of a multi-choice question, "Chat about this".
    fn rows(question: &Question) -> usize {
        question.options.len() + if question.multi_select { 3 } else { 2 }
    }

    fn move_tab(&mut self, forward: bool) {
        self.tab = if forward { (self.tab + 1).min(self.last_tab()) } else { self.tab.saturating_sub(1) };
        if let Some(state) = self.states.get_mut(self.tab) {
            state.moved = false;
            state.notes_mode = false;
            state.chat_focus = false;
            state.image_select = None;
        }
    }

    pub(crate) fn key(&mut self, key: Key) -> Step {
        self.notice = None;
        if self.tab < self.questions.len() && Self::previewed(&self.questions[self.tab]) {
            return self.preview_key(key);
        }
        if self.tab < self.questions.len() && self.states[self.tab].image_select.is_some() {
            self.image_key(&key);
            return Step::Stay;
        }
        if key == Key::Esc {
            return Step::Done(Outcome::Cancelled);
        }
        let typing_text = self.typing() && !self.states[self.tab].other.is_empty();
        match key {
            Key::Tab | Key::Right if !typing_text => {
                self.move_tab(true);
                return Step::Stay;
            }
            Key::BackTab | Key::Left if !typing_text => {
                self.move_tab(false);
                return Step::Stay;
            }
            _ => {}
        }
        if self.tab == self.questions.len() {
            return self.review_key(&key);
        }
        let question = self.questions[self.tab].clone();
        let n = question.options.len();
        let rows = Self::rows(&question);
        let state = &mut self.states[self.tab];
        match key {
            Key::Up => state.focus = state.focus.saturating_sub(1),
            Key::Down if state.focus == n && !state.images.is_empty() => state.image_select = Some(0),
            Key::Down => state.focus = (state.focus + 1).min(rows - 1),
            Key::CtrlG if state.focus == n && self.editor.is_some() => return Step::Edit(state.other.clone()),
            Key::CtrlV if state.focus == n => return Step::PasteImage,
            Key::Paste(text) => {
                state.focus = n;
                if text.is_empty() {
                    return if cfg!(target_os = "macos") { Step::PasteImage } else { Step::Stay };
                }
                state.other.push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                self.typed_other();
            }
            Key::Char(c) if state.focus == n && !c.is_control() => {
                state.other.push(c);
                self.typed_other();
            }
            Key::Backspace if state.focus == n => {
                state.other.pop();
                self.typed_other();
            }
            Key::Char(c @ '1'..='9') => {
                let row = c as usize - '1' as usize;
                if row == rows - 1 {
                    return Step::Done(self.clarify());
                }
                if row < n || (row == n && question.multi_select) {
                    state.focus = row;
                    return self.choose();
                }
                if row == n {
                    state.focus = n;
                }
            }
            Key::Char(' ') if question.multi_select => return self.choose(),
            Key::Enter => {
                if state.focus == rows - 1 {
                    return Step::Done(self.clarify());
                }
                return self.choose();
            }
            _ => {}
        }
        Step::Stay
    }

    /// Its own words tick a multi-choice question's last row.
    fn typed_other(&mut self) {
        let multi = self.questions[self.tab].multi_select;
        let state = &mut self.states[self.tab];
        if multi {
            state.other_checked = !state.other.is_empty();
            self.note_multi();
        }
    }

    /// Claude Code's attachments keys: → next, ← previous, Backspace removes,
    /// ↓ or Esc leaves.
    fn image_key(&mut self, key: &Key) {
        let state = &mut self.states[self.tab];
        let (Some(at), count) = (state.image_select, state.images.len()) else { return };
        match key {
            Key::Right if count > 1 => state.image_select = Some((at + 1) % count),
            Key::Left if count > 1 => state.image_select = Some((at + count - 1) % count),
            Key::Backspace | Key::Delete => {
                state.images.remove(at);
                state.image_select = if state.images.is_empty() { None } else { Some(at.min(state.images.len() - 1)) };
            }
            Key::Down | Key::Esc => state.image_select = None,
            _ => {}
        }
    }

    /// The side-by-side view's keys (Claude Code's preview question view).
    fn preview_key(&mut self, key: Key) -> Step {
        let n = self.questions[self.tab].options.len();
        let state = &mut self.states[self.tab];
        if state.chat_focus {
            match key {
                Key::Up => state.chat_focus = false,
                Key::Enter => return Step::Done(self.clarify()),
                Key::Esc => return Step::Done(Outcome::Cancelled),
                _ => {}
            }
            return Step::Stay;
        }
        if state.notes_mode {
            match key {
                Key::Esc => state.notes_mode = false,
                Key::Enter => {
                    state.notes_mode = false;
                    let notes = !state.notes.trim().is_empty();
                    if notes && state.moved {
                        let focus = state.focus;
                        return self.choose_previewed(focus);
                    }
                    if let Some(chosen) = state.chosen.clone() {
                        return self.answer(chosen);
                    }
                    if notes {
                        return self.answer(NOTES_ONLY.to_owned());
                    }
                }
                Key::Backspace => {
                    state.notes.pop();
                }
                Key::Char(c) if !c.is_control() => state.notes.push(c),
                Key::Paste(text) => state.notes.push_str(&text.replace("\r\n", "\n").replace('\r', "\n")),
                Key::CtrlG if self.editor.is_some() => return Step::Edit(state.notes.clone()),
                _ => {}
            }
            return Step::Stay;
        }
        match key {
            Key::Up if state.focus > 0 => {
                state.focus -= 1;
                state.moved = true;
            }
            Key::Down if state.focus + 1 == n => state.chat_focus = true,
            Key::Down => {
                state.focus += 1;
                state.moved = true;
            }
            Key::Enter => {
                let focus = state.focus;
                return self.choose_previewed(focus);
            }
            Key::Char('n') => state.notes_mode = true,
            Key::Esc => return Step::Done(Outcome::Cancelled),
            Key::Char(c @ '1'..='9') => {
                let row = c as usize - '1' as usize;
                if row < n && row != state.focus {
                    state.focus = row;
                    state.moved = true;
                }
            }
            Key::Tab | Key::Right => self.move_tab(true),
            Key::BackTab | Key::Left => self.move_tab(false),
            _ => {}
        }
        Step::Stay
    }

    fn choose_previewed(&mut self, at: usize) -> Step {
        let label = self.questions[self.tab].options[at].label.clone();
        let state = &mut self.states[self.tab];
        state.focus = at;
        state.chosen = Some(label.clone());
        self.answer(label)
    }

    /// A single-choice answer: submits a lone question, else moves on.
    fn answer(&mut self, answer: String) -> Step {
        self.answers[self.tab] = Some(answer);
        if self.single() {
            return Step::Done(self.submitted(None));
        }
        self.move_tab(true);
        Step::Stay
    }

    /// The editor's text, for the field Ctrl-G was pressed in.
    pub(crate) fn edited(&mut self, text: Option<String>) {
        let Some(text) = text else { return };
        let Some(state) = self.states.get_mut(self.tab) else { return };
        if state.notes_mode {
            state.notes = text;
        } else {
            state.other = text;
            self.typed_other();
        }
    }

    /// The clipboard's image for the question being answered, or why there
    /// is none.
    pub(crate) fn pasted_image(&mut self, image: Result<Pasted, String>) {
        let Some(state) = self.states.get_mut(self.tab) else { return };
        match image {
            Ok(image) => {
                let id = state.images.iter().map(|(id, _)| *id).max().unwrap_or(0) + 1;
                state.images.push((id, image));
                state.focus = self.questions[self.tab].options.len();
            }
            Err(why) => self.notice = Some(why),
        }
    }

    /// Enter (or a number) on the focused row.
    fn choose(&mut self) -> Step {
        let question = &self.questions[self.tab];
        let n = question.options.len();
        let state = &mut self.states[self.tab];
        if question.multi_select {
            match state.focus {
                f if f < n => state.checked[f] = !state.checked[f],
                f if f == n => state.other_checked = !state.other_checked && !state.other.is_empty(),
                // The Next / Submit button.
                _ => {
                    self.note_multi();
                    self.move_tab(true);
                    return Step::Stay;
                }
            }
            self.note_multi();
            return Step::Stay;
        }
        let with_images = !state.images.is_empty();
        let answer = match state.focus {
            f if f < n => question.options[f].label.clone(),
            _ if state.other.trim().is_empty() && !with_images => return Step::Stay,
            _ if state.other.trim().is_empty() => "(Image attached)".to_owned(),
            _ if with_images => format!("{} (Image attached)", state.other),
            _ => state.other.clone(),
        };
        state.chosen = Some(answer.clone());
        self.answer(answer)
    }

    /// A multi-choice question's answer: the ticked labels, then its own
    /// words when ticked.
    fn note_multi(&mut self) {
        let (question, state) = (&self.questions[self.tab], &self.states[self.tab]);
        let mut picked: Vec<&str> =
            question.options.iter().zip(&state.checked).filter(|(_, on)| **on).map(|(o, _)| o.label.as_str()).collect();
        if state.other_checked && !state.other.is_empty() {
            picked.push(&state.other);
        }
        self.answers[self.tab] = (!picked.is_empty()).then(|| picked.join(", "));
    }

    fn review_key(&mut self, key: &Key) -> Step {
        match key {
            Key::Up => self.review = 0,
            Key::Down => self.review = 1,
            Key::Char('1') => return Step::Done(self.submitted(None)),
            Key::Char('2') => return Step::Done(Outcome::Cancelled),
            Key::Enter if self.review == 0 => return Step::Done(self.submitted(None)),
            Key::Enter => return Step::Done(Outcome::Cancelled),
            _ => {}
        }
        Step::Stay
    }

    /// The answers as they stand, submitted by the user or (with `afk_ms`)
    /// by the idle timer.
    pub(crate) fn submitted(&self, afk_ms: Option<u64>) -> Outcome {
        let mut submission = Submission { afk_ms, ..Submission::default() };
        for ((question, answer), state) in self.questions.iter().zip(&self.answers).zip(&self.states) {
            if let Some(answer) = answer {
                submission.answers.push((question.question.clone(), answer.clone()));
            }
            if !Self::previewed(question) {
                continue;
            }
            let chosen = answer.as_deref().and_then(|a| question.options.iter().find(|o| o.label == a));
            let preview = match chosen.and_then(|o| preview_of(o.preview.as_deref())) {
                Some(Preview::Full(text)) => Some(text),
                _ => None,
            };
            let notes = Some(state.notes.trim().to_owned()).filter(|n| !n.is_empty());
            if preview.is_some() || notes.is_some() {
                submission.annotations.push((question.question.clone(), preview, notes));
            }
        }
        submission.images = self.images();
        Outcome::Submitted(submission)
    }

    fn images(&self) -> Vec<Pasted> {
        self.states.iter().flat_map(|s| s.images.iter().map(|(_, p)| p.clone())).collect()
    }

    fn clarify(&self) -> Outcome {
        let notes = self
            .questions
            .iter()
            .zip(&self.states)
            .map(|(q, s)| Some(s.notes.trim().to_owned()).filter(|n| Self::previewed(q) && !n.is_empty()))
            .collect();
        Outcome::Clarify { answers: self.answers.clone(), notes, images: self.images() }
    }

    /// The dialog for a terminal `width` columns wide and `height` rows high.
    pub(crate) fn lines(&self, width: usize, height: usize) -> Vec<Line> {
        let mut lines = vec![rule(width.min(40)), self.tabs(), Vec::new()];
        if self.tab == self.questions.len() {
            self.review_lines(&mut lines);
        } else if Self::previewed(&self.questions[self.tab]) {
            self.preview_lines(&mut lines, width, height);
        } else {
            self.question_lines(&mut lines);
        }
        if let Some(notice) = &self.notice {
            lines.push(vec![(notice.clone(), Style::Dim)]);
        }
        if let Some(seconds) = self.countdown {
            let text = format!("auto-continue in {seconds}s \u{b7} any key to stay");
            let pad = width.saturating_sub(text.width());
            lines.push(vec![(" ".repeat(pad), Style::Plain), (text, Style::Dim)]);
        }
        lines
    }

    /// `← ☐ Header  ☒ Answered  ✔ Submit →`, the current one marked.
    fn tabs(&self) -> Line {
        let single = self.single();
        let mut line: Line = Vec::new();
        if !single {
            line.push(("\u{2190} ".into(), if self.tab == 0 { Style::Dim } else { Style::Plain }));
        }
        for (i, q) in self.questions.iter().enumerate() {
            let mark = if self.answers[i].is_some() { BOX_ON } else { BOX_OFF };
            let header = if q.header.is_empty() { format!("Q{}", i + 1) } else { q.header.clone() };
            line.push((format!(" {mark} {header} "), if i == self.tab { Style::Current } else { Style::Plain }));
        }
        if !single {
            let at = self.tab == self.questions.len();
            line.push((format!(" {TICK} Submit "), if at { Style::Current } else { Style::Plain }));
            line.push((format!(" {ARROW_RIGHT}"), if at { Style::Dim } else { Style::Plain }));
        }
        line
    }

    fn footer(&self, parts: &[&str]) -> Line {
        vec![(parts.join(" \u{b7} "), Style::Dim)]
    }

    fn edit_hint(&self) -> Option<String> {
        self.editor.as_ref().map(|name| format!("ctrl+g to edit in {name}"))
    }

    fn question_lines(&self, lines: &mut Vec<Line>) {
        let (question, state) = (&self.questions[self.tab], &self.states[self.tab]);
        let n = question.options.len();
        let width = (n + 2).to_string().len() + 1;
        lines.push(vec![(question.question.clone(), Style::Bold)]);
        lines.push(Vec::new());
        let number = |i: usize| (format!("{:<width$} ", format!("{i}.")), Style::Dim);
        for (i, option) in question.options.iter().enumerate() {
            let focused = state.focus == i;
            let mut row = vec![pointer(focused), number(i + 1)];
            let label_style = if focused { Style::Accent } else { Style::Plain };
            if question.multi_select {
                let on = state.checked[i];
                row.push((
                    format!("[{}] ", if on { TICK } else { " " }),
                    if on { Style::Success } else { Style::Plain },
                ));
                row.push((option.label.clone(), label_style));
            } else {
                let chosen = state.chosen.as_deref() == Some(option.label.as_str());
                row.push((option.label.clone(), if chosen { Style::Success } else { label_style }));
                if chosen {
                    row.push((format!(" {TICK}"), Style::Success));
                }
            }
            lines.push(row);
            if !option.description.is_empty() {
                let indent = " ".repeat(2 + width + 1 + if question.multi_select { 4 } else { 0 });
                lines.push(vec![(indent, Style::Plain), (option.description.clone(), Style::Dim)]);
            }
        }
        let focused = state.focus == n;
        let mut row = vec![pointer(focused), number(n + 1)];
        if question.multi_select {
            let on = state.other_checked;
            row.push((format!("[{}] ", if on { TICK } else { " " }), if on { Style::Success } else { Style::Plain }));
        }
        if state.other.is_empty() {
            let placeholder = if question.multi_select { "Type something" } else { "Type something." };
            row.push((placeholder.into(), Style::Dim));
        } else {
            row.push((state.other.clone(), if focused { Style::Accent } else { Style::Plain }));
        }
        lines.push(row);
        if !state.images.is_empty() {
            let mut chips: Line = vec![(" ".repeat(2 + width + 1), Style::Plain)];
            for (i, (id, _)) in state.images.iter().enumerate() {
                let selected = state.image_select == Some(i);
                chips.push((format!("[Image #{id}]"), if selected { Style::Inverse } else { Style::Plain }));
                chips.push((" ".into(), Style::Plain));
            }
            let hint = match state.image_select {
                Some(_) if state.images.len() > 1 => {
                    "\u{2192} to next \u{b7} \u{2190} to prev \u{b7} Backspace to remove \u{b7} Esc to cancel"
                }
                Some(_) => "Backspace to remove \u{b7} Esc to cancel",
                None if focused => "(\u{2193} to select)",
                None => "",
            };
            chips.push((hint.into(), Style::Dim));
            lines.push(chips);
        }
        if question.multi_select {
            let on = state.focus == n + 1;
            let text = if self.tab + 1 == self.questions.len() { "Submit" } else { "Next" };
            lines.push(vec![pointer(on), ("   ".into(), Style::Plain), (text.into(), Style::Bold)]);
        }
        let chat = Self::rows(question) - 1;
        lines.push(rule(40));
        let on = state.focus == chat;
        lines.push(vec![
            pointer(on),
            (format!("{}. Chat about this", n + 2), if on { Style::Accent } else { Style::Plain }),
        ]);
        lines.push(Vec::new());
        let navigate =
            if self.questions.len() == 1 { "\u{2191}/\u{2193} to navigate" } else { "Tab/Arrow keys to navigate" };
        let edit = self.edit_hint().filter(|_| focused);
        let mut parts = vec!["Enter to select", navigate];
        parts.extend(edit.as_deref());
        parts.push("Esc to cancel");
        lines.push(self.footer(&parts));
    }

    /// Claude Code's preview view: options and the focused preview side by
    /// side, the notes under the preview.
    fn preview_lines(&self, lines: &mut Vec<Line>, width: usize, height: usize) {
        let (question, state) = (&self.questions[self.tab], &self.states[self.tab]);
        lines.push(vec![(question.question.clone(), Style::Bold)]);
        lines.push(Vec::new());
        let left: Vec<Line> = question
            .options
            .iter()
            .enumerate()
            .map(|(i, option)| {
                let focused = state.focus == i && !state.chat_focus;
                let chosen = state.chosen.as_deref() == Some(option.label.as_str());
                let style = if chosen {
                    Style::Success
                } else if focused {
                    Style::Accent
                } else {
                    Style::Plain
                };
                let mut row = vec![
                    if focused { (POINTER.into(), Style::Accent) } else { (" ".into(), Style::Plain) },
                    (format!(" {}.", i + 1), Style::Dim),
                    (format!(" {}", option.label), style),
                ];
                if chosen {
                    row.push((format!(" {TICK}"), Style::Success));
                }
                fit(&row, OPTIONS_WIDTH)
            })
            .collect();
        let max_width = width.saturating_sub(OPTIONS_WIDTH + GAP);
        let min_width = question
            .options
            .iter()
            .filter_map(|o| match preview_of(o.preview.as_deref()) {
                Some(Preview::Full(text)) => text.lines().map(|l| l.width()).max(),
                _ => None,
            })
            .fold(PREVIEW_MIN_WIDTH, usize::max);
        let content = match preview_of(question.options[state.focus].preview.as_deref()) {
            Some(Preview::Full(text)) => text,
            Some(Preview::Withheld) => WITHHELD.to_owned(),
            None => "No preview available".to_owned(),
        };
        let mut right = preview_box(&content, height.saturating_sub(26).max(1), min_width, max_width);
        right.push(Vec::new());
        let mut notes: Line = vec![("Notes:".into(), Style::Accent), (" ".into(), Style::Plain)];
        if state.notes_mode {
            if state.notes.is_empty() {
                notes.push(("Add notes on this design\u{2026}".into(), Style::Dim));
            } else {
                notes.push((state.notes.clone(), Style::Plain));
            }
        } else {
            let text = if state.notes.is_empty() { "press n to add notes" } else { state.notes.as_str() };
            notes.push((text.into(), Style::DimItalic));
        }
        right.push(fit(&notes, max_width.max(1)));
        for i in 0..left.len().max(right.len()) {
            let mut row = left.get(i).cloned().unwrap_or_default();
            let beside = right.get(i).cloned().unwrap_or_default();
            if !beside.is_empty() {
                let used: usize = row.iter().map(|(t, _)| t.width()).sum();
                row.push((" ".repeat(OPTIONS_WIDTH.saturating_sub(used) + GAP), Style::Plain));
                row.extend(beside);
            }
            lines.push(row);
        }
        lines.push(Vec::new());
        lines.push(rule(40));
        let on = state.chat_focus;
        lines.push(vec![
            if on { (POINTER.into(), Style::Accent) } else { (" ".into(), Style::Plain) },
            (" Chat about this".into(), if on { Style::Accent } else { Style::Plain }),
        ]);
        lines.push(Vec::new());
        let mut parts = vec!["Enter to select", "\u{2191}/\u{2193} to navigate", "n to add notes"];
        if self.questions.len() > 1 {
            parts.push("Tab to switch questions");
        }
        let edit = self.edit_hint().filter(|_| state.notes_mode);
        parts.extend(edit.as_deref());
        parts.push("Esc to cancel");
        lines.push(self.footer(&parts));
    }

    fn review_lines(&self, lines: &mut Vec<Line>) {
        lines.push(vec![("Review your answers".into(), Style::Bold)]);
        lines.push(Vec::new());
        if self.answers.iter().any(Option::is_none) {
            lines.push(vec![(format!("{WARNING} You have not answered all questions"), Style::Warning)]);
            lines.push(Vec::new());
        }
        let answered: Vec<_> =
            self.questions.iter().zip(&self.answers).filter_map(|(q, a)| Some((q, a.as_ref()?))).collect();
        for (question, answer) in &answered {
            lines.push(vec![(format!(" {}", question.question), Style::Plain)]);
            lines.push(vec![("   ".into(), Style::Plain), (format!("{ARROW_RIGHT} {answer}"), Style::Success)]);
        }
        if !answered.is_empty() {
            lines.push(Vec::new());
        }
        lines.push(vec![("Ready to submit your answers?".into(), Style::Dim)]);
        lines.push(Vec::new());
        for (i, label) in ["Submit answers", "Cancel"].into_iter().enumerate() {
            let on = self.review == i;
            lines.push(vec![
                pointer(on),
                (format!("{}. {label}", i + 1), if on { Style::Accent } else { Style::Plain }),
            ]);
        }
    }
}

fn pointer(on: bool) -> (String, Style) {
    if on { (format!("{POINTER} "), Style::Accent) } else { ("  ".into(), Style::Plain) }
}

fn rule(width: usize) -> Line {
    vec![("\u{2500}".repeat(width), Style::Dim)]
}

/// Claude Code's preview box: the text in a rounded-off frame at most
/// `max_width` wide (at least `min_width` inside), long lines wrapped, at
/// most `max_lines` lines with a "✂ N lines hidden" rule for the rest.
fn preview_box(content: &str, max_lines: usize, min_width: usize, max_width: usize) -> Vec<Line> {
    let wrap_at = max_width.saturating_sub(4).max(1);
    let all: Vec<String> = content.split('\n').flat_map(|line| hard_wrap(line, wrap_at)).collect();
    let cut = all.len() > max_lines;
    let shown = if cut { &all[..max_lines] } else { &all[..] };
    let widest = shown.iter().map(|l| l.width()).fold(min_width, usize::max);
    let outer = (widest + 4).min(max_width).max(4);
    let inner = outer - 4;
    let bar = "\u{2500}".repeat(outer - 2);
    let mut lines = vec![vec![(format!("\u{250c}{bar}\u{2510}"), Style::Dim)]];
    for line in shown {
        let text = if line.width() > inner { cut_to(line, inner) } else { line.clone() };
        let pad = " ".repeat(inner.saturating_sub(text.width()));
        lines.push(vec![
            ("\u{2502} ".into(), Style::Dim),
            (text, Style::Plain),
            (format!("{pad} \u{2502}"), Style::Dim),
        ]);
    }
    if cut {
        let label = format!(
            "\u{2500}\u{2500}\u{2500} \u{2702} \u{2500}\u{2500}\u{2500} {} lines hidden ",
            all.len() - max_lines
        );
        let rest = "\u{2500}".repeat((outer - 2).saturating_sub(label.width()));
        lines.push(vec![(format!("\u{251c}{label}{rest}\u{2524}"), Style::Warning)]);
    }
    lines.push(vec![(format!("\u{2514}{bar}\u{2518}"), Style::Dim)]);
    lines
}

fn hard_wrap(line: &str, width: usize) -> Vec<String> {
    if line.width() <= width {
        return vec![line.to_owned()];
    }
    let mut out = vec![String::new()];
    let mut used = 0;
    for c in line.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width {
            out.push(String::new());
            used = 0;
        }
        if let Some(last) = out.last_mut() {
            last.push(c);
        }
        used += w;
    }
    out
}

/// `text` cut to `width` columns, the last one an ellipsis.
fn cut_to(text: &str, width: usize) -> String {
    let mut cut = String::new();
    let mut used = 0;
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        cut.push(c);
        used += w;
    }
    cut.push('\u{2026}');
    cut
}

/// A line's text, cut to `width` columns.
pub(crate) fn fit(line: &Line, width: usize) -> Line {
    let mut left = width;
    let mut out = Vec::new();
    for (text, style) in line {
        if left == 0 {
            break;
        }
        if text.width() <= left {
            left -= text.width();
            out.push((text.clone(), *style));
            continue;
        }
        out.push((cut_to(text, left), *style));
        break;
    }
    out
}

/// The plain text of a line.
#[cfg(test)]
pub(crate) fn text(line: &Line) -> String {
    line.iter().map(|(t, _)| t.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use eos_agent_tools_session::QuestionOption;

    use super::*;

    fn question(text: &str, header: &str, multi_select: bool) -> Question {
        let option =
            |label: &str| QuestionOption { label: label.into(), description: format!("{label}!"), preview: None };
        Question {
            question: text.into(),
            header: header.into(),
            options: vec![option("Red"), option("Blue")],
            multi_select,
        }
    }

    fn shown(dialog: &Dialog) -> String {
        dialog.lines(80, 40).iter().map(text).collect::<Vec<_>>().join("\n")
    }

    fn keys(dialog: &mut Dialog, keys: &[Key]) -> Option<Outcome> {
        keys.iter().find_map(|k| match dialog.key(k.clone()) {
            Step::Done(outcome) => Some(outcome),
            _ => None,
        })
    }

    fn submitted(answers: &[(&str, &str)]) -> Option<Outcome> {
        let answers = answers.iter().map(|(q, a)| ((*q).to_owned(), (*a).to_owned())).collect();
        Some(Outcome::Submitted(Submission { answers, ..Submission::default() }))
    }

    #[test]
    fn one_question_submits_on_the_choice() {
        let mut d = Dialog::new(&[question("Which?", "Pick", false)]);
        let rule = "\u{2500}".repeat(40);
        assert_eq!(
            shown(&d),
            format!(
                "{rule}\n \u{2610} Pick \n\nWhich?\n\n\u{276f} 1. Red\n     Red!\n  2. Blue\n     Blue!\n  3. Type \
                 something.\n{rule}\n  4. Chat about this\n\nEnter to select \u{b7} \u{2191}/\u{2193} to navigate \u{b7} \
                 Esc to cancel"
            )
        );
        assert_eq!(keys(&mut d, &[Key::Down, Key::Enter]), submitted(&[("Which?", "Blue")]));
        let mut d = Dialog::new(&[question("Which?", "Pick", false)]);
        assert_eq!(keys(&mut d, &[Key::Char('1')]), submitted(&[("Which?", "Red")]));
        // An answer of one's own: typed on the last row.
        let mut d = Dialog::new(&[question("Which?", "Pick", false)]);
        assert_eq!(keys(&mut d, &[Key::Char('3'), Key::Enter]), None, "nothing typed yet");
        let typed = [Key::Char('g'), Key::Char('2'), Key::Char('x'), Key::Backspace, Key::Enter];
        assert_eq!(keys(&mut d, &typed), submitted(&[("Which?", "g2")]));
        let mut d = Dialog::new(&[question("Which?", "Pick", false)]);
        let clarify = Outcome::Clarify { answers: vec![None], notes: vec![None], images: Vec::new() };
        assert_eq!(keys(&mut d, &[Key::Down, Key::Down, Key::Down, Key::Enter]), Some(clarify));
        assert_eq!(Dialog::new(&[question("Which?", "Pick", false)]).key(Key::Esc), Step::Done(Outcome::Cancelled));
    }

    #[test]
    fn several_choices_tick_and_go_through_the_review() {
        let mut d = Dialog::new(&[question("Which?", "Pick", true)]);
        assert_eq!(keys(&mut d, &[Key::Char('1'), Key::Down, Key::Char(' '), Key::Down]), None);
        let s = shown(&d);
        assert!(s.contains("\u{2190}  \u{2612} Pick  \u{2714} Submit  \u{2192}"), "{s}");
        assert!(s.contains("  1. [\u{2714}] Red\n         Red!\n  2. [\u{2714}] Blue"), "{s}");
        assert!(s.contains("\u{276f} 3. [ ] Type something\n     Submit\n"), "{s}");
        // Its own words tick themselves.
        assert_eq!(keys(&mut d, &[Key::Char('x'), Key::Down, Key::Enter]), None);
        let s = shown(&d);
        assert!(s.contains("Review your answers\n\n Which?\n   \u{2192} Red, Blue, x\n"), "{s}");
        assert!(s.ends_with("Ready to submit your answers?\n\n\u{276f} 1. Submit answers\n  2. Cancel"), "{s}");
        assert_eq!(d.key(Key::Enter), Step::Done(submitted(&[("Which?", "Red, Blue, x")]).expect("some")));
    }

    #[test]
    fn several_questions_have_tabs() {
        let qs = [question("One?", "First", false), question("Two?", "", false)];
        let mut d = Dialog::new(&qs);
        assert!(shown(&d).contains("Enter to select \u{b7} Tab/Arrow keys to navigate"));
        assert_eq!(keys(&mut d, &[Key::Enter]), None, "the choice moves on");
        assert!(shown(&d).contains("\u{2190}  \u{2612} First  \u{2610} Q2  \u{2714} Submit  \u{2192}"));
        assert_eq!(keys(&mut d, &[Key::Right]), None, "skipped");
        let s = shown(&d);
        assert!(s.contains("\u{26a0} You have not answered all questions\n\n One?\n   \u{2192} Red\n"), "{s}");
        let clarify =
            Outcome::Clarify { answers: vec![Some("Red".into()), None], notes: vec![None, None], images: vec![] };
        assert_eq!(keys(&mut d, &[Key::Left, Key::Char('4')]), Some(clarify));
        let mut d = Dialog::new(&qs);
        assert_eq!(keys(&mut d, &[Key::Tab, Key::Tab, Key::Down, Key::Enter]), Some(Outcome::Cancelled));
    }

    fn previewed() -> Question {
        let mut q = question("Which layout?", "Layout", false);
        q.options[0].preview = Some("+----+\n| A  |\n+----+".into());
        q
    }

    /// Claude Code's preview view: options left, the focused preview boxed on
    /// the right, notes under it; the chosen preview and the notes go back.
    #[test]
    fn previews_sit_beside_the_options_with_notes() {
        let mut d = Dialog::new(&[previewed()]).with_editor(Some("Vim".into()));
        let s = shown(&d);
        let box_top = format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(42));
        assert!(s.contains(&format!("\u{276f} 1. Red{}{box_top}", " ".repeat(30 - 8 + 4))), "{s}");
        assert!(s.contains(&format!("  2. Blue{}\u{2502} +----+", " ".repeat(30 - 9 + 4))), "{s}");
        assert!(s.contains("Notes: press n to add notes"), "{s}");
        assert!(s.ends_with(
            "\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\
             \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\
             \u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\
             \u{2500}\n  Chat about this\n\nEnter to select \u{b7} \u{2191}/\u{2193} to navigate \u{b7} n to add notes \
             \u{b7} Esc to cancel"
        ));
        // The second option has none.
        assert_eq!(keys(&mut d, &[Key::Down]), None);
        assert!(shown(&d).contains("\u{2502} No preview available"));
        // Notes, edited in place or in the editor, then Enter takes the focused option.
        assert_eq!(keys(&mut d, &[Key::Up, Key::Char('n'), Key::Char('o')]), None);
        assert!(shown(&d).contains("ctrl+g to edit in Vim"));
        assert_eq!(d.key(Key::CtrlG), Step::Edit("o".into()));
        d.edited(Some("ok as is".into()));
        let Some(Outcome::Submitted(sub)) = keys(&mut d, &[Key::Enter]) else { panic!("submitted") };
        assert_eq!(sub.answers, [("Which layout?".to_owned(), "Red".to_owned())]);
        let preview = Some("+----+\n| A  |\n+----+".to_owned());
        assert_eq!(sub.annotations, [("Which layout?".to_owned(), preview, Some("ok as is".to_owned()))]);
        // Numbers only move the focus; Down past the last option reaches "Chat about this".
        let mut d = Dialog::new(&[previewed()]);
        assert_eq!(keys(&mut d, &[Key::Char('2'), Key::Char('n'), Key::Char('x'), Key::Esc]), None);
        let clarify = Outcome::Clarify { answers: vec![None], notes: vec![Some("x".into())], images: Vec::new() };
        assert_eq!(keys(&mut d, &[Key::Down, Key::Enter]), Some(clarify));
        // Notes alone answer "(notes only)".
        let mut d = Dialog::new(&[previewed()]);
        let Some(Outcome::Submitted(sub)) = keys(&mut d, &[Key::Char('n'), Key::Char('y'), Key::Enter]) else {
            panic!("notes only")
        };
        assert_eq!(sub.answers, [("Which layout?".to_owned(), NOTES_ONLY.to_owned())]);
    }

    #[test]
    fn a_long_preview_is_cut_with_a_rule() {
        let text = (1..=8).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let shown: Vec<String> = preview_box(&text, 3, 10, 30).iter().map(text_of).collect();
        assert_eq!(shown.len(), 6);
        assert_eq!(shown[1], "\u{2502} line 1     \u{2502}");
        assert!(
            shown[4].starts_with("\u{251c}\u{2500}\u{2500}\u{2500} \u{2702} \u{2500}\u{2500}\u{2500} 5 lines hidden")
        );
        assert!(shown[4].ends_with('\u{2524}'));
        assert_eq!(preview_of(Some(&"x".repeat(2001))), Some(Preview::Withheld));
        assert_eq!(preview_of(Some("  ")), None);
    }

    fn text_of(line: &Line) -> String {
        text(line)
    }

    /// Images pasted on the "Type something." row: chips under it, ↓ selects
    /// them, Backspace removes one; the answer says an image is attached.
    #[test]
    fn pasted_images_go_with_the_answer() {
        let png = Pasted { media_type: "image/png".into(), data: vec![1, 2] };
        let mut d = Dialog::new(&[question("Which?", "Pick", false)]);
        assert_eq!(keys(&mut d, &[Key::Char('3')]), None);
        assert_eq!(d.key(Key::CtrlV), Step::PasteImage);
        d.pasted_image(Err("No image found in clipboard. Use ctrl+v to paste images.".into()));
        assert!(shown(&d).contains("No image found in clipboard. Use ctrl+v to paste images."));
        d.pasted_image(Ok(png.clone()));
        d.pasted_image(Ok(png.clone()));
        assert!(shown(&d).contains("     [Image #1] [Image #2] (\u{2193} to select)"), "{}", shown(&d));
        assert_eq!(keys(&mut d, &[Key::Down, Key::Right, Key::Backspace]), None);
        assert!(shown(&d).contains("[Image #1] Backspace to remove"), "{}", shown(&d));
        assert_eq!(keys(&mut d, &[Key::Esc]), None, "Esc leaves the images, not the dialog");
        let Some(Outcome::Submitted(sub)) = keys(&mut d, &[Key::Char('h'), Key::Char('i'), Key::Enter]) else {
            panic!("submitted")
        };
        assert_eq!(sub.answers, [("Which?".to_owned(), "hi (Image attached)".to_owned())]);
        assert_eq!(sub.images, std::slice::from_ref(&png));
        // A paste elsewhere goes to the "Type something." row.
        let mut d = Dialog::new(&[question("Which?", "Pick", false)]);
        assert_eq!(keys(&mut d, &[Key::Paste("teal".into())]), None);
        assert!(shown(&d).contains("\u{276f} 3. teal"));
        assert_eq!(keys(&mut d, &[Key::Enter]), submitted(&[("Which?", "teal")]));
    }

    #[test]
    fn the_countdown_shows_at_the_right() {
        let mut d = Dialog::new(&[question("Which?", "Pick", false)]);
        d.set_countdown(Some(12));
        let lines = d.lines(60, 40);
        let last = text(lines.last().expect("line"));
        assert_eq!(last.trim_start(), "auto-continue in 12s \u{b7} any key to stay");
        assert_eq!(last.width(), 60);
    }

    #[test]
    fn lines_fit_the_terminal() {
        let line = vec![("ab".to_owned(), Style::Dim), ("cdef".to_owned(), Style::Bold)];
        assert_eq!(text(&fit(&line, 4)), "abc\u{2026}");
        assert_eq!(text(&fit(&line, 6)), "abcdef");
    }
}
