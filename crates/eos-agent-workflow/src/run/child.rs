//! `workflow(nameOrRef, args)`: a child workflow in a realm of its own.

use serde_json::Value;

use super::hooks::lane_of;
use super::{CHILD_MARK, Child, Event, Shared, lock, realm_config};
use crate::engine::HostError;
use crate::meta;
use crate::store::Readable;
use crate::{lint, named};

impl Shared {
    pub(super) fn child_failed(&self, realm: u32, parent: u64, name: &str, error_name: &str, message: &str) {
        let child = lock(&self.state).children.remove(&realm).unwrap_or(Child {
            parent,
            name: name.to_owned(),
            phase: format!("{CHILD_MARK} {name}"),
        });
        self.child_failed_known(&child, error_name, message);
    }

    pub(super) fn child_failed_known(&self, child: &Child, error_name: &str, shown: &str) {
        lock(&self.state).failures.push(format!("{}: {shown}", child.phase));
        self.log(format!("{CHILD_MARK} {} failed: {shown}", child.name));
        let message = shown.split_once(": ").map_or(shown, |(_, m)| m).lines().next().unwrap_or_default().to_owned();
        self.settle(0, child.parent, Err((error_name.to_owned(), message)));
    }

    /// `workflow(nameOrRef, args)`: one nesting level, sharing this run's caps.
    pub(super) fn child_workflow(&self, realm: u32, payload: Value) -> Result<String, HostError> {
        if realm != 0 {
            return Err(HostError::new(
                "workflow() cannot be called from within a child workflow \u{2014} nesting is limited to one level. \
                 Inline the inner script or call its agents directly.",
            ));
        }
        let (name, script) = match &payload["ref"] {
            Value::String(name) => {
                let found = named::find(name, self.spec.cwd.as_deref(), self.config.user_dir.as_deref());
                let Some(saved) = found else {
                    let names: Vec<String> = named::all(self.spec.cwd.as_deref(), self.config.user_dir.as_deref())
                        .into_iter()
                        .map(|s| s.name)
                        .collect();
                    let list = if names.is_empty() { "(none)".to_owned() } else { names.join(", ") };
                    return Err(HostError::new(format!(
                        "workflow('{name}'): no workflow with that name. Available: {list}"
                    )));
                };
                let parsed =
                    meta::parse(&saved.script).map_err(|e| HostError::new(format!("workflow('{name}'): {e}")))?;
                (saved.name, parsed.body)
            }
            Value::Object(r) => {
                let path = r.get("scriptPath").and_then(Value::as_str).unwrap_or_default();
                let readable = Readable { workspace: self.spec.workspace.as_deref(), session: &self.spec.dir };
                let (text, _) = readable
                    .load(path)
                    .map_err(|e| HostError::new(format!("workflow({{scriptPath: '{path}'}}): {e}")))?;
                let parsed = meta::parse(&text)
                    .map_err(|e| HostError::new(format!("workflow({{scriptPath: '{path}'}}): {e}")))?;
                (parsed.meta.name, parsed.body)
            }
            _ => {
                return Err(HostError::type_error(
                    "workflow() expects a workflow name (string) or {scriptPath: string}",
                ));
            }
        };
        if let Err(e) = self.config.engine.check_syntax(&script) {
            return Err(HostError::new(format!("workflow('{name}'): {e}")));
        }
        let refused = self
            .config
            .engine
            .ambiguous_slash(&script)
            .or_else(|| lint::forbidden(&script).map(std::borrow::Cow::into_owned));
        if let Some(refused) = refused {
            return Err(HostError::new(format!("workflow('{name}'): SyntaxError: {refused}")));
        }
        let mut state = lock(&self.state);
        let n = state.child_names.entry(name.clone()).or_default();
        *n += 1;
        let phase = if *n > 1 { format!("{CHILD_MARK} {name} #{n}") } else { format!("{CHILD_MARK} {name}") };
        self.reserve(&mut state, &phase);
        state.next_call += 1;
        let parent = state.next_call;
        let child_realm = state.next_realm;
        state.next_realm += 1;
        state.children.insert(child_realm, Child { parent, name: name.clone(), phase });
        drop(state);
        self.log(format!("{CHILD_MARK} running dynamic workflow {name}"));
        let config = realm_config(self.budget_total, payload.get("args"), Some(&name), lane_of(&payload));
        let _ = self.events.send(Event::OpenChild { realm: child_realm, parent, body: script, config, name });
        Ok(parent.to_string())
    }
}
