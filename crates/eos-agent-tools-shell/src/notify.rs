//! `<task-notification>` messages, the way Claude Code tells the model a
//! background command finished or a monitor saw something.

use std::path::Path;

#[derive(Debug, Default)]
pub(crate) struct Notification<'a> {
    pub task_id: &'a str,
    pub tool_use_id: Option<&'a str>,
    pub output_file: Option<&'a Path>,
    pub status: Option<&'a str>,
    /// Plain text; escaped here.
    pub summary: &'a str,
    /// Already-rendered extra elements (see [`event`] and [`note`]).
    pub body: String,
}

impl Notification<'_> {
    pub(crate) fn render(&self) -> String {
        let mut out = String::from("<task-notification>");
        let output_file = self.output_file.map(|p| p.to_string_lossy());
        let fields = [
            ("task-id", Some(self.task_id)),
            ("tool-use-id", self.tool_use_id),
            ("output-file", output_file.as_deref()),
            ("status", self.status),
            ("summary", Some(self.summary)),
        ];
        for (tag, value) in fields {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                out += &format!("\n<{tag}>{}</{tag}>", escape(value));
            }
        }
        out += &self.body;
        out += "\n</task-notification>";
        out
    }
}

/// A monitor's output (or notice) as the notification body.
pub(crate) fn event(text: &str) -> String {
    format!("\n<event>{}</event>", escape(text))
}

pub(crate) fn note(text: &str) -> String {
    format!("\n<note>{}</note>", escape(text))
}

pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_present_fields_in_order() {
        let n = Notification {
            task_id: "b1",
            tool_use_id: Some("toolu_1"),
            output_file: Some(Path::new("/s/tasks/b1.output")),
            status: Some("completed"),
            summary: "Background command \"a<b\" completed (exit code 0)",
            body: String::new(),
        };
        assert_eq!(
            n.render(),
            "<task-notification>\n<task-id>b1</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n\
             <output-file>/s/tasks/b1.output</output-file>\n<status>completed</status>\n\
             <summary>Background command \"a&lt;b\" completed (exit code 0)</summary>\n</task-notification>"
        );
        let e =
            Notification { task_id: "b2", summary: "Monitor event: \"x\"", body: event("line"), ..Default::default() };
        assert_eq!(
            e.render(),
            "<task-notification>\n<task-id>b2</task-id>\n<summary>Monitor event: \"x\"</summary>\n\
             <event>line</event>\n</task-notification>"
        );
    }
}
