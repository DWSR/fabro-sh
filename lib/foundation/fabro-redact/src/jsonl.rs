use serde_json::Value;

/// Returns true if a JSON key should be excluded from scanning/redaction.
///
/// Skips "signature" (exact), ID fields (ending in "id"/"ids"), and common
/// path/directory fields from agent transcripts.
fn should_skip_field(key: &str) -> bool {
    if key == "signature" {
        return true;
    }
    let lower = key.to_lowercase();

    // Skip ID fields
    if lower.ends_with("id") || lower.ends_with("ids") {
        return true;
    }

    // Skip common path and directory fields, plus identifier-like names
    matches!(
        lower.as_str(),
        "filepath" | "file_path" | "cwd" | "root" | "directory" | "dir" | "path" | "name"
    )
}

/// Returns true if the object has "type":"image", "type":"image_url", or
/// "type":"base64".
fn should_skip_object(obj: &serde_json::Map<String, Value>) -> bool {
    match obj.get("type").and_then(Value::as_str) {
        Some(t) => t.starts_with("image") || t == "base64",
        None => false,
    }
}

fn redact_json_tree(value: &mut Value, skip_field: bool) -> bool {
    if skip_field {
        return false;
    }
    let mut changed = false;
    match value {
        Value::Object(obj) => {
            if should_skip_object(obj) {
                return false;
            }
            for (key, child) in obj {
                changed |= redact_json_tree(child, should_skip_field(key));
            }
        }
        Value::Array(arr) => {
            for child in arr {
                changed |= redact_json_tree(child, false);
            }
        }
        Value::String(text) => {
            let redacted = super::redact_string(text);
            if redacted != *text {
                *text = redacted;
                changed = true;
            }
        }
        _ => {}
    }
    changed
}

pub fn redact_json_value(mut value: Value) -> Value {
    redact_json_tree(&mut value, false);
    value
}

/// Redact secrets in a single JSONL line.
///
/// Redacts the parsed JSON tree so skipped fields, object keys, and image
/// objects remain untouched even when they repeat a scanned string. Lines
/// with no redactions are returned unchanged, preserving original formatting.
///
/// Falls back to `redact_string` on invalid JSON.
pub fn redact_jsonl_line(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return line.to_string();
    }

    let mut parsed: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => return super::redact_string(line),
    };

    if !redact_json_tree(&mut parsed, false) {
        return line.to_string();
    }

    serde_json::to_string(&parsed).expect("a parsed JSON value should serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HIGH_ENTROPY_SECRET: &str = "sk-ant-api03-xK9mZ2vL8nQ5rT1wY4bC7dF0gH3jE6pA";

    #[test]
    fn skip_field_session_id() {
        assert!(should_skip_field("session_id"));
    }

    #[test]
    fn skip_field_content_not_skipped() {
        assert!(!should_skip_field("content"));
    }

    #[test]
    fn skip_field_file_path() {
        assert!(should_skip_field("file_path"));
    }

    #[test]
    fn skip_field_filepath() {
        assert!(should_skip_field("filePath"));
    }

    #[test]
    fn skip_field_id_variants() {
        assert!(should_skip_field("id"));
        assert!(should_skip_field("sessionId"));
        assert!(should_skip_field("checkpoint_id"));
        assert!(should_skip_field("checkpointID"));
        assert!(should_skip_field("userIds"));
    }

    #[test]
    fn skip_field_path_variants() {
        assert!(should_skip_field("cwd"));
        assert!(should_skip_field("root"));
        assert!(should_skip_field("directory"));
        assert!(should_skip_field("dir"));
        assert!(should_skip_field("path"));
    }

    #[test]
    fn skip_field_name() {
        assert!(should_skip_field("name"));
    }

    #[test]
    fn skip_field_false_positives() {
        assert!(!should_skip_field("content"));
        assert!(!should_skip_field("type"));
        assert!(!should_skip_field("text"));
        assert!(!should_skip_field("output"));
        assert!(!should_skip_field("video"));
        assert!(!should_skip_field("identify"));
        assert!(!should_skip_field("signatures"));
        assert!(!should_skip_field("consideration"));
    }

    #[test]
    fn redact_json_value_preserves_name_field() {
        let input = serde_json::json!({
            "name": "fabro-01KQR3V9D4VPFFWMNTVH09J48G",
            "content": format!("token={HIGH_ENTROPY_SECRET}"),
        });

        let redacted = redact_json_value(input);

        assert_eq!(redacted["name"], "fabro-01KQR3V9D4VPFFWMNTVH09J48G");
        assert_eq!(redacted["content"], "token=REDACTED");
    }

    #[test]
    fn skip_object_image_type() {
        let obj: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{"type": "image", "data": "base64data"}"#).unwrap();
        assert!(should_skip_object(&obj));
    }

    #[test]
    fn skip_object_text_type_not_skipped() {
        let obj: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{"type": "text", "content": "hello"}"#).unwrap();
        assert!(!should_skip_object(&obj));
    }

    #[test]
    fn skip_object_no_type() {
        let obj: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{"content": "hello"}"#).unwrap();
        assert!(!should_skip_object(&obj));
    }

    #[test]
    fn skip_object_image_url() {
        let obj: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{"type": "image_url"}"#).unwrap();
        assert!(should_skip_object(&obj));
    }

    #[test]
    fn skip_object_base64() {
        let obj: serde_json::Map<String, Value> =
            serde_json::from_str(r#"{"type": "base64"}"#).unwrap();
        assert!(should_skip_object(&obj));
    }

    #[test]
    fn redact_jsonl_line_no_secrets() {
        let input = r#"{"type":"text","content":"hello"}"#;
        assert_eq!(redact_jsonl_line(input), input);
    }

    #[test]
    fn redact_jsonl_line_preserves_ids_repeated_in_scanned_values() {
        // This run ID from a failed CLI attach exceeds the entropy threshold.
        // Redacting an event's context value must not also rewrite its envelope.
        let run_id = "01M5228EHCT0P73Z4BYGDRWJNS";
        let input = serde_json::json!({
            "run_id": run_id,
            "item": { "context": { "value": run_id } },
        });
        let result = redact_jsonl_line(&input.to_string());
        let redacted: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(redacted["run_id"], run_id);
        assert_eq!(redacted["item"]["context"]["value"], "REDACTED");
        assert_eq!(redacted, redact_json_value(input));
    }

    #[test]
    fn redact_jsonl_line_preserves_keys_and_skipped_objects_with_shared_values() {
        let input = serde_json::json!({
            HIGH_ENTROPY_SECRET: "unchanged key",
            "content": HIGH_ENTROPY_SECRET,
            "image": { "type": "image", "data": HIGH_ENTROPY_SECRET },
            "path": HIGH_ENTROPY_SECRET,
            "session_ids": [HIGH_ENTROPY_SECRET],
        });
        let redacted: Value = serde_json::from_str(&redact_jsonl_line(&input.to_string())).unwrap();

        assert_eq!(redacted[HIGH_ENTROPY_SECRET], "unchanged key");
        assert_eq!(redacted["content"], "REDACTED");
        assert_eq!(redacted["image"]["data"], HIGH_ENTROPY_SECRET);
        assert_eq!(redacted["path"], HIGH_ENTROPY_SECRET);
        assert_eq!(redacted["session_ids"][0], HIGH_ENTROPY_SECRET);
        assert_eq!(redacted, redact_json_value(input));
    }

    #[test]
    fn redact_jsonl_line_scans_decoded_strings() {
        let input = format!(r#"{{"content":"\u0073{}"}}"#, &HIGH_ENTROPY_SECRET[1..]);
        let redacted: Value = serde_json::from_str(&redact_jsonl_line(&input)).unwrap();
        assert_eq!(redacted["content"], "REDACTED");
    }

    #[test]
    fn redact_jsonl_line_preserves_formatting_without_redactions() {
        let input = "  { \"content\" : \"hello\", \"number\": 1.00 }\n";
        assert_eq!(redact_jsonl_line(input), input);
    }

    #[test]
    fn redact_json_value_matches_jsonl_behavior() {
        let input = serde_json::json!({
            "content": format!("key={HIGH_ENTROPY_SECRET}"),
            "session_id": HIGH_ENTROPY_SECRET,
        });

        let redacted = redact_json_value(input);

        assert_eq!(redacted["content"], "key=REDACTED");
        assert_eq!(redacted["session_id"], HIGH_ENTROPY_SECRET);
    }

    #[test]
    fn redact_jsonl_line_with_secret_in_content() {
        let input = format!(r#"{{"type":"text","content":"key={HIGH_ENTROPY_SECRET}"}}"#);
        let result = redact_jsonl_line(&input);
        assert!(
            result.contains("REDACTED"),
            "expected REDACTED in: {result}"
        );
        assert!(
            !result.contains(HIGH_ENTROPY_SECRET),
            "secret should be redacted: {result}"
        );
    }

    #[test]
    fn redact_jsonl_line_preserves_session_id() {
        let input = format!(r#"{{"session_id":"{HIGH_ENTROPY_SECRET}","content":"normal text"}}"#);
        let result = redact_jsonl_line(&input);
        assert!(
            result.contains(HIGH_ENTROPY_SECRET),
            "session_id should be preserved: {result}"
        );
        assert!(
            !result.contains("REDACTED"),
            "should have no redactions: {result}"
        );
    }

    #[test]
    fn redact_jsonl_line_fallback_on_invalid_json() {
        let input = format!(r#"{{"type":"text", "invalid {HIGH_ENTROPY_SECRET} json"#);
        let result = redact_jsonl_line(&input);
        assert!(
            result.contains("REDACTED"),
            "expected REDACTED in: {result}"
        );
    }

    #[test]
    fn redact_jsonl_line_preserves_file_path_field() {
        let input = r#"{"file_path":"/private/var/folders/v4/31cd3cg52_sfrpb1mbtr7q7r0000gn/T/test/controller.go","content":"normal text"}"#;
        let result = redact_jsonl_line(input);
        assert!(
            result.contains("/private/var/folders"),
            "file_path should be preserved: {result}"
        );
        assert!(
            !result.contains("REDACTED"),
            "should have no redactions: {result}"
        );
    }

    #[test]
    fn redact_jsonl_line_secrets_in_content_not_in_paths() {
        let input =
            format!(r#"{{"file_path":"/tmp/test.go","content":"api_key={HIGH_ENTROPY_SECRET}"}}"#);
        let result = redact_jsonl_line(&input);
        assert!(
            result.contains("/tmp/test.go"),
            "file_path should be preserved: {result}"
        );
        assert!(
            !result.contains(HIGH_ENTROPY_SECRET),
            "secret in content should be redacted: {result}"
        );
        assert!(
            result.contains("REDACTED"),
            "expected REDACTED in: {result}"
        );
    }

    #[test]
    fn redact_jsonl_line_image_object_skipped() {
        let input = format!(r#"{{"type":"image","data":"{HIGH_ENTROPY_SECRET}"}}"#);
        let result = redact_jsonl_line(&input);
        assert!(
            !result.contains("REDACTED"),
            "image data should not be redacted: {result}"
        );
    }

    #[test]
    fn redact_jsonl_line_empty_line() {
        assert_eq!(redact_jsonl_line(""), "");
        assert_eq!(redact_jsonl_line("  "), "  ");
    }
}
