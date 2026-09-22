//! Pure helpers for requests that must remain visible in the terminal UI.
//!
//! `answer` returns the result payload, not the JSON-RPC envelope. The caller
//! submits it only after user input and handles unsupported methods with
//! `reply_error`. Parse errors can be displayed so the user can correct input.

use serde_json::{json, Map, Value};

/// Describe a request without truncation. The UI owns wrapping and scrolling.
/// Control characters (except line feeds) and bidi controls are escaped here.
pub fn describe(request: &Value) -> String {
    let method = request["method"].as_str().unwrap_or("<missing method>");
    let instructions = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            "Enter yes to approve once, no to decline, or cancel to interrupt the turn.\n\
             Only available decisions are accepted; session and policy grants are unsupported."
        }
        "item/permissions/requestApproval" => {
            "Enter yes to grant exactly the requested permissions for this turn, or no to grant none."
        }
        "item/tool/requestUserInput" => {
            "For one question, enter text or an option number (starting at 1).\n\
             For multiple questions, enter JSON keyed by every question ID, for example:\n\
             {\"question_id\": \"answer\", \"another_id\": [1, \"other answer\"]}"
        }
        "mcpServer/elicitation/request" => {
            "Enter no to decline or cancel to cancel.\n\
             For a form, submit a JSON object containing your answers.\n\
             For a URL, visit it and complete the requested action, then enter yes to confirm.\n\
             This client does not open URLs or verify completion."
        }
        _ => "Unsupported request. Submit input to report an error to the server; use a client that supports this method.",
    };
    let params = &request["params"];
    let mut text = format!("{method}\n{instructions}\n");
    for (key, label) in [
        ("command", "Command"),
        ("cwd", "Working directory"),
        ("reason", "Reason"),
        ("networkApprovalContext", "Network context"),
        ("changes", "File changes"),
        ("grantRoot", "Requested write root"),
        ("permissions", "Requested permissions"),
        ("availableDecisions", "Available decisions"),
        ("serverName", "MCP server"),
        ("message", "Message"),
        ("url", "URL"),
    ] {
        if let Some(value) = params.get(key).filter(|v| !v.is_null()) {
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| pretty(value));
            text.push_str(&format!("\n{label}: {value}\n"));
        }
    }
    if let Some(questions) = params["questions"].as_array() {
        for question in questions {
            text.push_str(&format!(
                "\n[{}] {}: {}\n",
                question["id"].as_str().unwrap_or("<missing id>"),
                question["header"].as_str().unwrap_or("Question"),
                question["question"].as_str().unwrap_or("")
            ));
            if let Some(options) = question["options"].as_array() {
                for (index, option) in options.iter().enumerate() {
                    text.push_str(&format!(
                        "  {}. {} — {}\n",
                        index + 1,
                        option["label"].as_str().unwrap_or("<missing label>"),
                        option["description"].as_str().unwrap_or("")
                    ));
                }
            }
        }
    }
    text.push_str(&format!("\nFull request:\n{}", pretty(request)));
    terminal_text(&text)
}

/// Construct a response only from an explicit, valid user submission.
pub fn answer(request: &Value, input: &str) -> Result<Value, String> {
    let params = &request["params"];
    match request["method"].as_str().unwrap_or("<missing method>") {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let decision = match input.trim().to_ascii_lowercase().as_str() {
                "yes" | "y" => "accept",
                "no" | "n" => "decline",
                "cancel" => "cancel",
                _ => return Err("Enter yes, no, or cancel. Empty input never approves a request.".into()),
            };
            // Older schemas omit this field. A supplied list is authoritative:
            // never substitute acceptForSession or an amendment for accept.
            if let Some(available) = params.get("availableDecisions").filter(|v| !v.is_null()) {
                let available = available.as_array().ok_or("Invalid availableDecisions: expected an array; no decision sent.")?;
                if !available.iter().any(|value| value.as_str() == Some(decision)) {
                    return Err(format!(
                        "Decision {decision:?} is not available. Choose a supported offered decision: {}. Session and policy grants require another client.",
                        terminal_text(&pretty(&params["availableDecisions"]))
                    ));
                }
            }
            Ok(json!({"decision": decision}))
        }
        "item/permissions/requestApproval" => match input.trim().to_ascii_lowercase().as_str() {
            "yes" | "y" => {
                let permissions = params["permissions"].as_object().ok_or("Missing or invalid requested permissions; no grant sent.")?;
                Ok(json!({"permissions": permissions, "scope": "turn"}))
            }
            "no" | "n" => Ok(json!({"permissions": {}, "scope": "turn"})),
            _ => Err("Enter yes to grant the requested permissions for this turn, or no to grant none.".into()),
        },
        "item/tool/requestUserInput" => answer_questions(params, input),
        "mcpServer/elicitation/request" => answer_elicitation(params, input),
        method => Err(format!(
            "Unsupported server request {}. Send a JSON-RPC method-not-found error with reply_error; use a client that supports this method.",
            terminal_text(method)
        )),
    }
}

fn answer_questions(params: &Value, input: &str) -> Result<Value, String> {
    let questions = params["questions"]
        .as_array()
        .filter(|q| !q.is_empty())
        .ok_or("Request has no questions to answer.")?;
    let keyed = if questions.len() > 1 || input.trim_start().starts_with('{') {
        let value: Value = serde_json::from_str(input)
            .map_err(|error| format!("Enter a JSON object keyed by question ID: {error}"))?;
        Some(
            value
                .as_object()
                .ok_or("Expected a JSON object keyed by question ID.")?
                .clone(),
        )
    } else {
        None
    };
    let mut answers = Map::new();
    for question in questions {
        let id = question["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or("Question is missing a nonempty ID; cannot route its answer.")?;
        if answers.contains_key(id) {
            return Err(format!(
                "Duplicate question ID {}; cannot route answers safely.",
                terminal_text(id)
            ));
        }
        let value = match &keyed {
            Some(values) => values.get(id).cloned().ok_or_else(|| {
                format!(
                    "Missing answer for question {}. Include every question ID.",
                    terminal_text(id)
                )
            })?,
            None => Value::String(input.to_owned()),
        };
        let values = match value {
            Value::Array(values) => values,
            value => vec![value],
        };
        if values.is_empty() {
            return Err(format!(
                "Enter at least one answer for question {}.",
                terminal_text(id)
            ));
        }
        let values = values
            .iter()
            .map(|value| question_answer(question, value))
            .collect::<Result<Vec<_>, _>>()?;
        answers.insert(id.to_owned(), json!({"answers": values}));
    }
    if let Some(values) = keyed {
        if let Some(id) = values.keys().find(|id| !answers.contains_key(*id)) {
            return Err(format!(
                "Unknown question ID {}. Use only the displayed question IDs.",
                terminal_text(id)
            ));
        }
    }
    Ok(json!({"answers": answers}))
}

fn question_answer(question: &Value, value: &Value) -> Result<String, String> {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Number(number) if number.is_u64() => number.to_string(),
        _ => return Err("Answers must be text, option numbers, or arrays of those values.".into()),
    };
    if text.trim().is_empty() {
        return Err("Enter an answer; empty input does not select an option.".into());
    }
    if let Some(options) = question["options"]
        .as_array()
        .filter(|options| !options.is_empty())
    {
        // Digits denote a one-based option index, including out-of-range input.
        if text.trim().bytes().all(|byte| byte.is_ascii_digit()) {
            let option = text
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|index| index.checked_sub(1))
                .and_then(|index| options.get(index));
            return option
                .and_then(|option| option["label"].as_str())
                .map(str::to_owned)
                .ok_or_else(|| {
                    format!(
                        "Choose an option number from 1 to {}, or enter text.",
                        options.len()
                    )
                });
        }
    }
    Ok(text)
}

fn answer_elicitation(params: &Value, input: &str) -> Result<Value, String> {
    match input.trim().to_ascii_lowercase().as_str() {
        "no" | "n" => return Ok(json!({"action": "decline"})),
        "cancel" => return Ok(json!({"action": "cancel"})),
        _ => {}
    }
    match params["mode"].as_str() {
        Some("url") => {
            if input.trim().eq_ignore_ascii_case("yes") || input.trim().eq_ignore_ascii_case("y") {
                if params["url"].as_str().filter(|url| !url.trim().is_empty()).is_none() {
                    return Err("URL elicitation is missing its URL; cannot confirm it.".into());
                }
                // This acknowledges the user's explicit confirmation only. It
                // does not assert that this helper opened or completed the URL.
                Ok(json!({"action": "accept"}))
            } else {
                Err("Complete the action at the displayed URL, then enter yes to confirm, no to decline, or cancel.".into())
            }
        }
        Some("form" | "openai/form" | "openaiForm") => {
            let content: Value = serde_json::from_str(input)
                .map_err(|error| format!("Submit form answers as a JSON object, or enter no/cancel: {error}"))?;
            if !content.is_object() {
                return Err("Form answers must be a JSON object matching requestedSchema.".into());
            }
            // Forward only user-supplied content. Schema validation remains the
            // eliciting server's responsibility; do not invent defaults.
            Ok(json!({"action": "accept", "content": content}))
        }
        _ => Err("Unsupported elicitation mode. Enter no/cancel, or use a client supporting the requested mode.".into()),
    }
}

fn pretty(value: &Value) -> String {
    // Serializing serde_json::Value cannot fail for supported JSON values.
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

fn terminal_text(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        if (character.is_control() && character != '\n')
            || matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(method: &str, params: Value) -> Value {
        json!({"id": 42, "method": method, "params": params})
    }

    #[test]
    fn approvals_require_explicit_input_and_never_grant_for_session() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
        ] {
            let request = make_request(method, json!({"grantRoot": "/work"}));
            for input in [
                "",
                " ",
                "acceptForSession",
                "always",
                "{}",
                "yes for session",
            ] {
                assert!(answer(&request, input).is_err(), "{method}: {input}");
            }
            assert_eq!(
                answer(&request, " YES ").unwrap(),
                json!({"decision": "accept"})
            );
            assert_eq!(
                answer(&request, "no").unwrap(),
                json!({"decision": "decline"})
            );
            assert_eq!(
                answer(&request, "cancel").unwrap(),
                json!({"decision": "cancel"})
            );
        }
    }

    #[test]
    fn available_decisions_are_authoritative() {
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
        ] {
            let request = make_request(
                method,
                json!({"availableDecisions": [
                    "acceptForSession", "decline",
                    {"acceptWithExecpolicyAmendment": {"execpolicy_amendment": ["git"]}},
                    {"applyNetworkPolicyAmendment": {"network_policy_amendment": {"host": "example.org", "action": "allow"}}}
                ]}),
            );
            assert!(answer(&request, "yes").is_err());
            assert!(answer(&request, "cancel").is_err());
            assert_eq!(
                answer(&request, "no").unwrap(),
                json!({"decision": "decline"})
            );
            for available in [json!([]), json!("accept"), json!([{"accept": true}])] {
                let request = make_request(method, json!({"availableDecisions": available}));
                assert!(answer(&request, "yes").is_err());
            }
            let request = make_request(method, json!({"availableDecisions": ["accept", "cancel"]}));
            assert_eq!(
                answer(&request, "yes").unwrap(),
                json!({"decision": "accept"})
            );
            assert_eq!(
                answer(&request, "cancel").unwrap(),
                json!({"decision": "cancel"})
            );
            assert!(answer(&request, "no").is_err());
        }
    }

    #[test]
    fn permissions_copy_the_exact_request_for_one_turn() {
        let permissions = json!({"network": {"enabled": true}, "fileSystem": {
            "entries": [{"access": "write", "path": {"type": "path", "path": "/work/output"}}],
            "read": ["/work/source"], "globScanMaxDepth": 3
        }});
        let request = make_request(
            "item/permissions/requestApproval",
            json!({"permissions": permissions, "scope": "session"}),
        );
        assert!(answer(&request, "").is_err());
        assert!(answer(&request, "session").is_err());
        assert_eq!(
            answer(&request, "yes").unwrap(),
            json!({"permissions": permissions, "scope": "turn"})
        );
        assert_eq!(
            answer(&request, "no").unwrap(),
            json!({"permissions": {}, "scope": "turn"})
        );
        let malformed = make_request("item/permissions/requestApproval", json!({}));
        assert!(answer(&malformed, "yes").is_err());
    }

    fn questions() -> Value {
        make_request(
            "item/tool/requestUserInput",
            json!({"questions": [
                {"id": "color", "header": "Color", "question": "Choose a color", "options": [
                    {"label": "Red", "description": "Warm"},
                    {"label": "Blue", "description": "Cool"}
                ]},
                {"id": "notes", "header": "Notes", "question": "Any notes?", "options": null}
            ]}),
        )
    }

    #[test]
    fn multiple_questions_keep_their_ids_and_map_each_option() {
        assert_eq!(
            answer(&questions(), r#"{"notes":["first","second"],"color":2}"#).unwrap(),
            json!({
                "answers": {"color": {"answers": ["Blue"]}, "notes": {"answers": ["first", "second"]}}
            })
        );
        for input in [
            "",
            "yes",
            "{}",
            r#"{"color":1}"#,
            r#"{"color":1,"notes":"ok","extra":"oops"}"#,
            r#"{"color":1,"notes":[]}"#,
            r#"{"color":1,"notes":false}"#,
        ] {
            assert!(answer(&questions(), input).is_err(), "{input}");
        }
    }

    #[test]
    fn single_question_accepts_text_and_one_based_option_numbers() {
        let mut request = questions();
        request["params"]["questions"]
            .as_array_mut()
            .unwrap()
            .truncate(1);
        assert_eq!(
            answer(&request, "2").unwrap(),
            json!({"answers": {"color": {"answers": ["Blue"]}}})
        );
        assert_eq!(
            answer(&request, "custom answer").unwrap(),
            json!({"answers": {"color": {"answers": ["custom answer"]}}})
        );
        for input in ["", " ", "0", "3", "999999999999999999999999999999999999"] {
            assert!(answer(&request, input).is_err());
        }
        request["params"]["questions"][0]["options"] = Value::Null;
        assert_eq!(
            answer(&request, "123").unwrap(),
            json!({"answers": {"color": {"answers": ["123"]}}})
        );
    }

    #[test]
    fn missing_and_duplicate_question_ids_are_rejected() {
        let mut request = questions();
        request["params"]["questions"][1]["id"] = json!("color");
        assert!(answer(&request, r#"{"color":"answer"}"#)
            .unwrap_err()
            .contains("Duplicate"));
        request["params"]["questions"][1]["id"] = Value::Null;
        assert!(answer(&request, r#"{"color":"answer"}"#).is_err());
    }

    #[test]
    fn elicitation_requires_actual_form_content_or_explicit_url_confirmation() {
        for mode in ["form", "openai/form", "openaiForm"] {
            let request = make_request("mcpServer/elicitation/request", json!({"mode": mode}));
            for input in ["", "yes", "null", "[]", "{broken"] {
                assert!(answer(&request, input).is_err());
            }
            assert_eq!(
                answer(&request, r#"{"name":"Ada","count":3}"#).unwrap(),
                json!({"action": "accept", "content": {"name": "Ada", "count": 3}})
            );
            assert_eq!(
                answer(&request, "no").unwrap(),
                json!({"action": "decline"})
            );
            assert_eq!(
                answer(&request, "cancel").unwrap(),
                json!({"action": "cancel"})
            );
        }
        let request = make_request(
            "mcpServer/elicitation/request",
            json!({"mode": "url", "url": "https://example.org/auth"}),
        );
        for input in ["", "{}", "done"] {
            assert!(answer(&request, input).is_err());
        }
        assert_eq!(
            answer(&request, "yes").unwrap(),
            json!({"action": "accept"})
        );
        assert_eq!(
            answer(&request, "no").unwrap(),
            json!({"action": "decline"})
        );
        assert_eq!(
            answer(&request, "cancel").unwrap(),
            json!({"action": "cancel"})
        );
    }

    #[test]
    fn unknown_requests_never_fake_success() {
        for method in ["item/tool/call", "future/requestApproval", ""] {
            let request = make_request(method, json!({}));
            for input in ["", "yes", "no", "cancel", "{}"] {
                let error = answer(&request, input).unwrap_err();
                assert!(error.contains("Unsupported server request"));
                assert!(error.contains("reply_error"));
            }
        }
    }

    #[test]
    fn descriptions_keep_full_context_and_escape_terminal_controls() {
        let change = format!("{}END_OF_DIFF", "line\n".repeat(300));
        let request = make_request(
            "item/commandExecution/requestApproval",
            json!({
                "command": "printf '\u{1b}[2J'\r\u{7}\u{85}\u{202e}", "cwd": "/work/project",
                "reason": "Fetch dependencies", "networkApprovalContext": {"host": "example.org", "protocol": "https"},
                "changes": [{"path": "src/main.rs", "diff": change}], "unrecognizedContext": "keep this too"
            }),
        );
        let text = describe(&request);
        for expected in [
            "Working directory: /work/project",
            "Fetch dependencies",
            "example.org",
            "END_OF_DIFF",
            "keep this too",
            "Full request:",
        ] {
            assert!(text.contains(expected), "{expected}");
        }
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
        assert!(!text.contains('\u{202e}'));
        let text = describe(&questions());
        assert!(text.contains("[color] Color: Choose a color"));
        assert!(text.contains("2. Blue — Cool"));
        assert!(text.contains("[notes] Notes: Any notes?"));
    }
}
