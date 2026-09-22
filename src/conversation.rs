use serde_json::Value;
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    User,
    Answer,
    Activity,
    Notice,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub kind: Kind,
    pub label: String,
    pub text: String,
    approval_details: Option<String>,
}

/// Display state only. Never feeds filtered content back to the Codex engine.
#[derive(Default)]
pub struct Conversation {
    pub entries: Vec<Entry>,
    indices: HashMap<String, usize>,
    pub revision: u64,
}

impl Conversation {
    pub fn notice(&mut self, text: impl Into<String>) {
        self.entries.push(Entry {
            kind: Kind::Notice,
            label: "Notice".into(),
            text: text.into(),
            approval_details: None,
        });
        self.revision += 1;
    }

    pub fn visible(&self, expanded: bool) -> impl Iterator<Item = &Entry> {
        self.entries
            .iter()
            .filter(move |entry| expanded || entry.kind != Kind::Activity)
    }

    pub fn hidden_count(&self) -> usize {
        self.entries
            .iter()
            .filter(|e| e.kind == Kind::Activity)
            .count()
    }

    /// Decision context only. Streamed output and tool results stay in activity.
    pub fn details(&self, turn_id: &str, item_id: &str) -> Option<&str> {
        self.indices
            .get(&Self::key(turn_id, item_id))
            .and_then(|&index| self.entries[index].approval_details.as_deref())
    }

    pub fn restore(&mut self, thread: &Value) {
        if let Some(turns) = thread["turns"].as_array() {
            for turn in turns {
                if let Some(items) = turn["items"].as_array() {
                    for item in items {
                        self.item(string(&turn["id"]), item);
                    }
                }
            }
        }
    }

    pub fn event(&mut self, message: &Value) {
        let params = &message["params"];
        match string(&message["method"]) {
            "item/started" | "item/completed" => {
                self.item(string(&params["turnId"]), &params["item"]);
            }
            "item/agentMessage/delta" | "item/plan/delta" => {
                self.delta(params, Kind::Answer, "Codex");
            }
            "item/commandExecution/outputDelta" => {
                self.delta(params, Kind::Activity, "Command");
            }
            "item/reasoning/summaryTextDelta" | "item/reasoning/textDelta" => {
                self.delta(params, Kind::Activity, "Reasoning");
            }
            "error" => self.notice(error_text(&params["error"])),
            "warning" => self.notice(string(&params["message"])),
            "configWarning" => self.notice(format!(
                "{}\n{}",
                string(&params["summary"]),
                string(&params["details"])
            )),
            _ => {}
        }
    }

    fn key(turn_id: &str, id: &str) -> String {
        format!("{turn_id}/{id}")
    }

    fn item(&mut self, turn_id: &str, item: &Value) {
        let id = string(&item["id"]);
        if id.is_empty() {
            return;
        }
        let key = Self::key(turn_id, id);
        let (kind, label, text) = match string(&item["type"]) {
            "userMessage" => (Kind::User, "You".into(), user_text(&item["content"])),
            "agentMessage" => (
                if item["phase"] == "commentary" {
                    Kind::Activity
                } else {
                    Kind::Answer
                },
                "Codex".into(),
                string(&item["text"]).to_owned(),
            ),
            "plan" => (
                Kind::Answer,
                "Plan".into(),
                string(&item["text"]).to_owned(),
            ),
            "exitedReviewMode" => (
                Kind::Answer,
                "Review".into(),
                string(&item["review"]).to_owned(),
            ),
            "commandExecution" => (
                Kind::Activity,
                format!("Command · {}", string(&item["status"])),
                format!(
                    "$ {}\n{}\n{}",
                    string(&item["command"]),
                    string(&item["cwd"]),
                    string(&item["aggregatedOutput"])
                ),
            ),
            name => (
                Kind::Activity,
                name.to_owned(),
                serde_json::to_string_pretty(item).unwrap_or_default(),
            ),
        };
        let approval_details = match string(&item["type"]) {
            "commandExecution" => Some(serde_json::json!({
                "command": item["command"], "cwd": item["cwd"]
            })),
            "fileChange" => Some(serde_json::json!({"changes": item["changes"]})),
            _ => None,
        }
        .map(|details| serde_json::to_string_pretty(&details).unwrap_or_default());
        let entry = Entry {
            kind,
            label,
            text,
            approval_details,
        };
        if let Some(&index) = self.indices.get(&key) {
            self.entries[index] = entry;
        } else {
            self.indices.insert(key, self.entries.len());
            self.entries.push(entry);
        }
        self.revision += 1;
    }

    fn delta(&mut self, params: &Value, kind: Kind, label: &str) {
        let key = Self::key(string(&params["turnId"]), string(&params["itemId"]));
        let index = *self.indices.entry(key).or_insert_with(|| {
            self.entries.push(Entry {
                kind,
                label: label.into(),
                text: String::new(),
                approval_details: None,
            });
            self.entries.len() - 1
        });
        self.entries[index].text.push_str(string(&params["delta"]));
        self.revision += 1;
    }
}

pub fn string(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

pub fn error_text(value: &Value) -> String {
    value["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn user_text(content: &Value) -> String {
    content
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|item| match string(&item["type"]) {
                    "text" => string(&item["text"]).to_owned(),
                    "image" => "[Image]".into(),
                    "localImage" => format!("[Image: {}]", string(&item["path"])),
                    _ => item.to_string(),
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn send(c: &mut Conversation, item: Value) {
        c.event(&json!({"method":"item/completed","params":{"turnId":"t","item":item}}));
    }

    #[test]
    fn approval_context_preserves_file_changes_without_exposing_tool_results() {
        let mut c = Conversation::default();
        let changes = json!([{"path":"src/lib.rs", "kind":{"type":"update"},
            "diff":"-old\n+new\n"}]);
        send(
            &mut c,
            json!({"type":"fileChange", "id":"patch", "changes":changes,
            "output":"PRIVATE_TOOL_OUTPUT"}),
        );
        assert_eq!(
            serde_json::from_str::<Value>(c.details("t", "patch").unwrap()).unwrap(),
            json!({"changes":changes})
        );
        send(
            &mut c,
            json!({"type":"mcpToolCall", "id":"mcp",
            "result":{"text":"PRIVATE_MCP_RESULT"}}),
        );
        assert!(c.details("t", "mcp").is_none());
        assert!(c.visible(false).next().is_none());
        assert!(c
            .visible(true)
            .any(|e| e.text.contains("PRIVATE_MCP_RESULT")));
    }

    #[test]
    fn native_and_mcp_output_hidden_without_changing_source_events() {
        let mut c = Conversation::default();
        send(
            &mut c,
            json!({"type":"userMessage","id":"q","content":[{"type":"text","text":"test my code"}]}),
        );
        let tool = json!({"type":"commandExecution","id":"cmd","command":"cargo test","aggregatedOutput":"private tool output"});
        send(&mut c, tool.clone());
        send(
            &mut c,
            json!({"type":"mcpToolCall","id":"mcp","result":{"text":"mcp noise"}}),
        );
        send(
            &mut c,
            json!({"type":"agentMessage","id":"progress","phase":"commentary","text":"Running tests"}),
        );
        send(
            &mut c,
            json!({"type":"agentMessage","id":"answer","phase":"final_answer","text":"Tests passed."}),
        );
        assert_eq!(
            c.visible(false)
                .map(|e| e.text.as_str())
                .collect::<Vec<_>>(),
            ["test my code", "Tests passed."]
        );
        assert!(c
            .visible(true)
            .any(|e| e.text.contains("private tool output")));
        assert_eq!(c.hidden_count(), 3);
        assert_eq!(tool["aggregatedOutput"], "private tool output");
    }

    #[test]
    fn long_tool_run_never_evicts_previous_question_and_answer() {
        let mut c = Conversation::default();
        send(
            &mut c,
            json!({"type":"agentMessage","id":"previous","text":"Earlier answer"}),
        );
        for n in 0..10_000 {
            send(
                &mut c,
                json!({"type":"commandExecution","id":format!("tool-{n}"),"aggregatedOutput":"noise\n".repeat(100)}),
            );
        }
        assert_eq!(c.visible(false).count(), 1);
        assert_eq!(c.visible(false).next().unwrap().text, "Earlier answer");
        assert_eq!(c.hidden_count(), 10_000);
    }

    #[test]
    fn completed_item_replaces_deltas_and_resume_reuses_same_filter() {
        let mut c = Conversation::default();
        send(
            &mut c,
            json!({"type":"agentMessage","id":"a","phase":"commentary","text":""}),
        );
        c.event(&json!({"method":"item/agentMessage/delta","params":{"turnId":"t","itemId":"a","delta":"working"}}));
        assert_eq!(c.visible(false).count(), 0);
        send(
            &mut c,
            json!({"type":"agentMessage","id":"a","phase":"final_answer","text":"Done"}),
        );
        assert_eq!(c.visible(false).next().unwrap().text, "Done");
        assert_eq!(c.entries.len(), 1);
        c.restore(&json!({"turns":[{"id":"old","items":[{"type":"commandExecution","id":"c","command":"ls"},{"type":"agentMessage","id":"answer","text":"Old answer"}]}]}));
        assert_eq!(c.visible(false).count(), 2);
        c.event(&json!({"method":"error","params":{"error":{"message":"Connection failed"}}}));
        assert!(c.visible(false).any(|e| e.text == "Connection failed"));
    }
}
