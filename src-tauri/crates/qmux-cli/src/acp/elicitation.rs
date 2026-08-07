//! Schema handling and URL vetting for ACP elicitation.
//!
//! `elicitation/create` is how an agent asks the user a structured question.
//! Two modes: **form**, a flat schema of primitives the client renders as
//! fields, and **url**, an out-of-band flow (OAuth and the like) the client
//! opens in a browser.
//!
//! The split matters for more than rendering. The spec forbids form mode for
//! secrets — passwords, API keys, tokens, private keys, payment credentials —
//! and forbids falling back to it for them, precisely because a form answer
//! travels back through the agent. URL mode exists so the value never does.
//!
//! Everything here is pure so the prompting in `mod.rs` stays thin and this
//! stays testable without a terminal.

use serde_json::{Map, Value};

/// One question in a form-mode elicitation.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub key: String,
    pub kind: FieldKind,
    pub required: bool,
    pub default: Option<Value>,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    Text,
    /// A string with a fixed set of allowed values.
    Choice(Vec<String>),
    Number {
        integer: bool,
    },
    Bool,
}

/// Reads a `requestedSchema` into an ordered field list.
///
/// ACP restricts these schemas to a flat object of primitives and string enums,
/// so anything nested is a protocol violation rather than something to render
/// badly. Field order is alphabetical: a parsed JSON object has no order to
/// preserve, so required fields are floated to the front to give the form some
/// shape rather than leaving it arbitrary.
pub fn parse_schema(schema: &Value) -> Result<Vec<Field>, String> {
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("requestedSchema must be an object schema".to_string());
    }
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .ok_or_else(|| "requestedSchema has no properties".to_string())?;
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut fields: Vec<Field> = properties
        .iter()
        .filter_map(|(key, property)| {
            Some(Field {
                key: key.clone(),
                kind: field_kind(property)?,
                required: required.contains(&key.as_str()),
                default: property.get("default").cloned(),
                description: property
                    .get("description")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            })
        })
        .collect();
    if fields.is_empty() {
        return Err("requestedSchema has no usable fields".to_string());
    }
    fields.sort_by_key(|field| (!field.required, field.key.clone()));
    Ok(fields)
}

fn field_kind(property: &Value) -> Option<FieldKind> {
    if let Some(values) = property.get("enum").and_then(Value::as_array) {
        let choices: Vec<String> = values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        // An enum with nothing selectable is not a field, it's a dead end.
        return (!choices.is_empty()).then_some(FieldKind::Choice(choices));
    }
    match property.get("type").and_then(Value::as_str)? {
        "string" => Some(FieldKind::Text),
        "number" => Some(FieldKind::Number { integer: false }),
        "integer" => Some(FieldKind::Number { integer: true }),
        "boolean" => Some(FieldKind::Bool),
        // Objects and arrays are outside what the spec allows here; skipping
        // one field is better than refusing the whole form.
        _ => None,
    }
}

/// Turns typed input into the JSON value the field declares.
pub fn coerce(field: &Field, input: &str) -> Result<Value, String> {
    let input = input.trim();
    match &field.kind {
        FieldKind::Text => Ok(Value::String(input.to_string())),
        FieldKind::Choice(choices) => {
            // Accept either the 1-based index shown in the prompt or the value
            // typed out; both are natural and neither is ambiguous, since a
            // choice list is never numeric.
            if let Ok(index) = input.parse::<usize>()
                && index >= 1
                && index <= choices.len()
            {
                return Ok(Value::String(choices[index - 1].clone()));
            }
            choices
                .iter()
                .find(|choice| choice.eq_ignore_ascii_case(input))
                .map(|choice| Value::String(choice.clone()))
                .ok_or_else(|| format!("enter 1-{} or one of the listed values", choices.len()))
        }
        FieldKind::Number { integer: true } => input
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| "enter a whole number".to_string()),
        FieldKind::Number { integer: false } => input
            .parse::<f64>()
            .ok()
            .and_then(serde_json::Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| "enter a number".to_string()),
        FieldKind::Bool => match input.to_ascii_lowercase().as_str() {
            "y" | "yes" | "true" | "1" => Ok(Value::Bool(true)),
            "n" | "no" | "false" | "0" => Ok(Value::Bool(false)),
            _ => Err("enter y or n".to_string()),
        },
    }
}

/// The one-line prompt for a field: its name, choices, and default.
pub fn prompt_label(field: &Field) -> String {
    let mut label = field.key.clone();
    if let FieldKind::Bool = field.kind {
        label.push_str(" (y/n)");
    }
    if let Some(default) = &field.default {
        let rendered = match default {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        label.push_str(&format!(" [{rendered}]"));
    } else if !field.required {
        label.push_str(" (optional)");
    }
    label
}

/// Assembles the answers, applying defaults and reporting a required field the
/// user skipped. Returned as the `content` of an `accept`.
pub fn build_content(field_answers: Vec<(Field, Option<Value>)>) -> Result<Value, String> {
    let mut content = Map::new();
    for (field, answer) in field_answers {
        let value = answer.or_else(|| field.default.clone());
        match value {
            Some(value) => {
                content.insert(field.key, value);
            }
            None if field.required => {
                return Err(format!("'{}' is required", field.key));
            }
            // An optional field left blank is absent, not empty — the agent
            // asked whether it was set, and "" is a different answer to that.
            None => {}
        }
    }
    Ok(Value::Object(content))
}

/// Field names that must never be collected through a form. The agent is the
/// one bound by that rule, but it is the client that would leak the value, so
/// this warns rather than trusting.
const SECRET_HINTS: [&str; 8] = [
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "credential",
    "private_key",
];

pub fn secret_looking_fields(fields: &[Field]) -> Vec<String> {
    fields
        .iter()
        .filter(|field| {
            let key = field.key.to_ascii_lowercase();
            SECRET_HINTS.iter().any(|hint| key.contains(hint))
        })
        .map(|field| field.key.clone())
        .collect()
}

/// Whatever is worth saying out loud before offering to open `url`.
///
/// The spec requires the client to show the full URL and get explicit consent;
/// these are the cases where consent should be an informed no. Punycode is
/// called out by name because a homograph domain is exactly the attack a
/// rendered URL hides.
pub fn url_warnings(url: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let lower = url.to_ascii_lowercase();

    if !lower.starts_with("https://") {
        warnings.push(if lower.starts_with("http://") {
            "this link is plain HTTP, not HTTPS".to_string()
        } else {
            "this link is not a web URL".to_string()
        });
    }
    let host = host_of(&lower).unwrap_or_default();
    if host
        .split('.')
        .any(|label| label.starts_with("xn--") || label.starts_with("XN--"))
    {
        warnings.push(format!(
            "the domain '{host}' uses punycode and may not be the site it appears to be"
        ));
    }
    if lower.contains('@') && host.contains('@') {
        warnings.push("the link embeds credentials in its host".to_string());
    }
    warnings
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "strategy": { "type": "string", "enum": ["conservative", "aggressive"],
                              "description": "how bold to be" },
                "runs": { "type": "integer", "default": 3 },
                "ratio": { "type": "number" },
                "dry": { "type": "boolean" },
                "note": { "type": "string" },
            },
            "required": ["strategy", "ratio"],
        })
    }

    fn field(schema: &Value, key: &str) -> Field {
        parse_schema(schema)
            .expect("parses")
            .into_iter()
            .find(|field| field.key == key)
            .unwrap_or_else(|| panic!("no field {key}"))
    }

    #[test]
    fn a_schema_becomes_fields_with_required_ones_first() {
        let fields = parse_schema(&schema()).expect("parses");
        assert_eq!(fields.len(), 5);
        // Required first, then alphabetical within each group.
        let order: Vec<&str> = fields.iter().map(|field| field.key.as_str()).collect();
        assert_eq!(order, ["ratio", "strategy", "dry", "note", "runs"]);
        assert!(fields[0].required && fields[1].required);
        assert!(!fields[2].required);
    }

    #[test]
    fn every_primitive_kind_is_recognised() {
        let schema = schema();
        assert_eq!(
            field(&schema, "strategy").kind,
            FieldKind::Choice(vec!["conservative".into(), "aggressive".into()])
        );
        assert_eq!(
            field(&schema, "runs").kind,
            FieldKind::Number { integer: true }
        );
        assert_eq!(
            field(&schema, "ratio").kind,
            FieldKind::Number { integer: false }
        );
        assert_eq!(field(&schema, "dry").kind, FieldKind::Bool);
        assert_eq!(field(&schema, "note").kind, FieldKind::Text);
        assert_eq!(field(&schema, "runs").default, Some(json!(3)));
        assert_eq!(
            field(&schema, "strategy").description.as_deref(),
            Some("how bold to be")
        );
    }

    #[test]
    fn unsupported_property_types_are_skipped_not_fatal() {
        let fields = parse_schema(&json!({
            "type": "object",
            "properties": {
                "nested": { "type": "object" },
                "list": { "type": "array" },
                "ok": { "type": "string" },
            },
        }))
        .expect("the usable field survives");
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].key, "ok");
    }

    #[test]
    fn malformed_schemas_are_rejected_with_a_reason() {
        for (schema, needle) in [
            (json!({ "type": "string" }), "object schema"),
            (json!({ "type": "object" }), "no properties"),
            (
                json!({ "type": "object", "properties": { "x": { "type": "array" } } }),
                "no usable fields",
            ),
            (
                json!({ "type": "object", "properties": { "x": { "enum": [] } } }),
                "no usable fields",
            ),
        ] {
            let err = parse_schema(&schema).expect_err("should reject");
            assert!(err.contains(needle), "{err} should mention {needle}");
        }
    }

    #[test]
    fn a_choice_accepts_its_index_or_its_value() {
        let field = field(&schema(), "strategy");
        assert_eq!(coerce(&field, "1").unwrap(), json!("conservative"));
        assert_eq!(coerce(&field, "2").unwrap(), json!("aggressive"));
        assert_eq!(coerce(&field, "AGGRESSIVE").unwrap(), json!("aggressive"));
        assert!(coerce(&field, "3").is_err());
        assert!(coerce(&field, "sideways").is_err());
    }

    #[test]
    fn numbers_and_booleans_are_validated_not_stringified() {
        let schema = schema();
        assert_eq!(coerce(&field(&schema, "runs"), " 7 ").unwrap(), json!(7));
        assert!(coerce(&field(&schema, "runs"), "7.5").is_err());
        assert_eq!(
            coerce(&field(&schema, "ratio"), "0.25").unwrap(),
            json!(0.25)
        );
        assert!(coerce(&field(&schema, "ratio"), "many").is_err());

        for (input, expected) in [
            ("y", true),
            ("Yes", true),
            ("1", true),
            ("n", false),
            ("false", false),
        ] {
            assert_eq!(
                coerce(&field(&schema, "dry"), input).unwrap(),
                json!(expected)
            );
        }
        assert!(coerce(&field(&schema, "dry"), "maybe").is_err());
        // NaN and infinity have no JSON representation.
        assert!(coerce(&field(&schema, "ratio"), "NaN").is_err());
        assert!(coerce(&field(&schema, "ratio"), "inf").is_err());
    }

    #[test]
    fn prompts_show_choices_defaults_and_optionality() {
        let schema = schema();
        assert_eq!(prompt_label(&field(&schema, "runs")), "runs [3]");
        assert_eq!(prompt_label(&field(&schema, "dry")), "dry (y/n) (optional)");
        assert_eq!(prompt_label(&field(&schema, "note")), "note (optional)");
        // A required field with no default gets no annotation.
        assert_eq!(prompt_label(&field(&schema, "ratio")), "ratio");
    }

    #[test]
    fn content_applies_defaults_and_omits_blank_optionals() {
        let schema = schema();
        let content = build_content(vec![
            (field(&schema, "ratio"), Some(json!(0.5))),
            (field(&schema, "runs"), None),
            (field(&schema, "note"), None),
        ])
        .expect("builds");

        assert_eq!(content["ratio"], json!(0.5));
        assert_eq!(content["runs"], json!(3), "the default fills in");
        assert!(
            content.get("note").is_none(),
            "an optional blank is absent, not empty string"
        );
    }

    #[test]
    fn a_required_field_with_no_answer_and_no_default_is_an_error() {
        let err = build_content(vec![(field(&schema(), "ratio"), None)])
            .expect_err("required and unanswered");
        assert!(err.contains("'ratio' is required"), "{err}");
    }

    #[test]
    fn fields_that_look_like_secrets_are_flagged() {
        let fields = parse_schema(&json!({
            "type": "object",
            "properties": {
                "api_key": { "type": "string" },
                "GitHubToken": { "type": "string" },
                "username": { "type": "string" },
            },
        }))
        .expect("parses");
        let flagged = secret_looking_fields(&fields);
        assert!(flagged.contains(&"api_key".to_string()));
        assert!(flagged.contains(&"GitHubToken".to_string()));
        assert!(!flagged.contains(&"username".to_string()));
    }

    #[test]
    fn https_urls_pass_unremarked() {
        assert!(url_warnings("https://github.com/login/oauth?x=1").is_empty());
    }

    #[test]
    fn insecure_and_deceptive_urls_are_called_out() {
        assert!(url_warnings("http://example.com")[0].contains("plain HTTP"));
        assert!(url_warnings("ftp://example.com")[0].contains("not a web URL"));

        // A homograph domain is the whole reason the spec demands the full URL
        // be shown; rendering alone would not reveal it.
        let warnings = url_warnings("https://xn--80ak6aa92e.com/login");
        assert!(
            warnings.iter().any(|warning| warning.contains("punycode")),
            "{warnings:?}"
        );

        let warnings = url_warnings("https://user:pw@evil.example/login");
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("credentials")),
            "{warnings:?}"
        );
    }
}
