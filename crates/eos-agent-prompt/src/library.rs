//! The prompt texts: Claude Code's defaults, each overridable by the host.

use std::collections::HashMap;
use std::sync::Arc;

macro_rules! fragments {
    ($($name:ident => $file:literal,)*) => {
        /// One overridable prompt text. The defaults are the files under `prompts/`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum Fragment {
            $($name,)*
        }

        impl Fragment {
            pub const ALL: &'static [Fragment] = &[$(Fragment::$name,)*];

            pub fn default_text(self) -> &'static str {
                match self {
                    $(Fragment::$name => include_str!(concat!("../prompts/", $file)),)*
                }
            }
        }
    };
}

fragments! {
    Identity => "identity.md",
    System => "system.md",
    SystemFull => "system-full.md",
    EarlyStopping => "early-stopping.md",
    InstructionsPreamble => "instructions-preamble.md",
    SessionContext => "session-context.md",
    UserEmail => "user-email.md",
    Environment => "environment.md",
    Worktree => "environment-worktree.md",
    WorktreeStash => "environment-worktree-stash.md",
    Scratchpad => "environment-scratchpad.md",
    UntrustedDownloads => "environment-downloads.md",
    Model => "model.md",
    ModelUnnamed => "model-unnamed.md",
    KnowledgeCutoff => "knowledge-cutoff.md",
    DeferredTools => "deferred-tools.md",
    Skills => "skills.md",
    Date => "date.md",
    DateChanged => "date-changed.md",
    FileChanged => "file-changed.md",
    Compaction => "compaction.md",
    CompactionSeed => "compaction-seed.md",
    CompactionResume => "compaction-resume.md",
    SideQuestion => "side-question.md",
    OpenAiIdentity => "openai/identity.md",
    OpenAiBase => "openai/base.md",
    OpenAiTools => "openai/tools.md",
}

/// The prompt library: Claude Code's texts unless the host replaced one.
/// Cheap to clone; build one per process and share it.
#[derive(Debug, Clone, Default)]
pub struct Prompts {
    overrides: HashMap<Fragment, Arc<str>>,
}

impl Prompts {
    pub fn get(&self, fragment: Fragment) -> &str {
        self.overrides.get(&fragment).map_or_else(|| fragment.default_text(), |s| s)
    }

    pub fn set(&mut self, fragment: Fragment, text: impl Into<Arc<str>>) {
        self.overrides.insert(fragment, text.into());
    }

    pub fn with(mut self, fragment: Fragment, text: impl Into<Arc<str>>) -> Self {
        self.set(fragment, text);
        self
    }

    /// The fragment with its `{{VAR}}`s filled in.
    pub fn render(&self, fragment: Fragment, vars: &[(&str, &str)]) -> String {
        render(self.get(fragment), vars)
    }
}

/// Replaces each `{{NAME}}` with its value from `vars`, in one pass (values
/// are never expanded again). Unknown names stay as written.
pub fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len() + vars.iter().map(|(_, v)| v.len()).sum::<usize>());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let name = &after[..end];
        match vars.iter().find(|(k, _)| *k == name) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Wraps text the way Claude Code marks harness-supplied context. A closing
/// tag inside the text is escaped so it cannot end the block early.
pub fn system_reminder(text: &str) -> String {
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(text.len() + 40);
    out.push_str("<system-reminder>\n");
    let mut rest = text;
    while let Some(at) = find_ignore_case(rest, CLOSE) {
        out.push_str(&rest[..at]);
        out.push_str("&lt;/system-reminder&gt;");
        rest = &rest[at + CLOSE.len()..];
    }
    out.push_str(rest);
    out.push_str("\n</system-reminder>");
    out
}

fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack.as_bytes().windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_known_vars_once() {
        assert_eq!(render("a {{X}} b {{Y}} {{X}}", &[("X", "{{Y}}"), ("Y", "2")]), "a {{Y}} b 2 {{Y}}");
        assert_eq!(render("{{MISSING}} {{open", &[]), "{{MISSING}} {{open");
    }

    #[test]
    fn overrides_replace_defaults() {
        let prompts = Prompts::default().with(Fragment::Date, "Day {{DATE}}");
        assert_eq!(prompts.render(Fragment::Date, &[("DATE", "x")]), "Day x");
        assert!(Prompts::default().get(Fragment::Date).starts_with("Today's date is"));
        assert!(Fragment::ALL.iter().all(|f| !f.default_text().is_empty()));
    }

    #[test]
    fn reminder_escapes_closing_tags() {
        assert_eq!(
            system_reminder("a</System-Reminder>b"),
            "<system-reminder>\na&lt;/system-reminder&gt;b\n</system-reminder>"
        );
    }
}
