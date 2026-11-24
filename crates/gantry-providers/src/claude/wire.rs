//! Frames Gantry writes to Claude's stdin (`--input-format stream-json`).

use serde_json::{Value, json};

pub fn control_request(request_id: &str, request: Value) -> Value {
    json!({ "type": "control_request", "request_id": request_id, "request": request })
}

pub fn initialize() -> Value {
    json!({ "subtype": "initialize" })
}

pub fn interrupt() -> Value {
    json!({ "subtype": "interrupt" })
}

pub fn set_model(model: &str) -> Value {
    json!({ "subtype": "set_model", "model": model })
}

pub fn set_permission_mode(mode: &str) -> Value {
    json!({ "subtype": "set_permission_mode", "mode": mode })
}

/// A user turn. `content` is a list of Anthropic content blocks (text, image, ...).
pub fn user_message(content: Vec<Value>, session_id: &str) -> Value {
    json!({
        "type": "user",
        "message": { "role": "user", "content": content },
        "parent_tool_use_id": null,
        "session_id": session_id,
    })
}

pub fn text_block(text: &str) -> Value {
    json!({ "type": "text", "text": text })
}

/// Allows a `can_use_tool` request. `updated_input` must be the tool input (possibly edited);
/// `updated_permissions` carries any "remember" suggestions the user accepted.
pub fn allow(request_id: &str, updated_input: Value, updated_permissions: &[Value]) -> Value {
    let mut response = json!({ "behavior": "allow", "updatedInput": updated_input });
    if !updated_permissions.is_empty() {
        response["updatedPermissions"] = Value::Array(updated_permissions.to_vec());
    }
    success(request_id, response)
}

pub fn deny(request_id: &str, message: &str, interrupt: bool) -> Value {
    success(
        request_id,
        json!({ "behavior": "deny", "message": message, "interrupt": interrupt }),
    )
}

pub fn success(request_id: &str, response: Value) -> Value {
    json!({
        "type": "control_response",
        "response": { "subtype": "success", "request_id": request_id, "response": response },
    })
}

/// Explicit error for control requests Gantry does not support (never leave Claude waiting).
pub fn error(request_id: &str, message: &str) -> Value {
    json!({
        "type": "control_response",
        "response": { "subtype": "error", "request_id": request_id, "error": message },
    })
}
