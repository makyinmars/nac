//! Bounded, exact-value redaction at the Claude process boundary.

use serde_json::Value;

const MAX_EVENT_FIELD_CHARS: usize = 16_000;

#[derive(Clone)]
pub(crate) struct ClaudeOutputRedactor {
    secrets: Vec<String>,
}

impl ClaudeOutputRedactor {
    pub(crate) fn from_environment() -> Self {
        let secrets = std::env::vars_os()
            .filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
            .filter(|(name, value)| !value.is_empty() && credential_name(name))
            .map(|(_, value)| value)
            .collect();
        Self { secrets }
    }

    pub(crate) fn text(&self, value: &str, max_chars: usize) -> String {
        let secrets = self.secrets.iter().map(String::as_str).collect::<Vec<_>>();
        crate::model::redact_credentials(value, &secrets)
            .chars()
            .take(max_chars)
            .collect()
    }

    pub(crate) fn event(&self, value: Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.text(&text, MAX_EVENT_FIELD_CHARS)),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|item| self.event(item)).collect())
            }
            Value::Object(fields) => Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| (key, self.event(value)))
                    .collect(),
            ),
            other => other,
        }
    }
}

fn credential_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.contains("TOKEN")
        || upper.contains("SECRET")
        || upper.contains("PASSWORD")
        || upper.ends_with("_KEY")
        || upper.contains("API_KEY")
        || upper.contains("CREDENTIAL")
}

pub(crate) fn sanitize_output_text(text: &str, max_chars: usize) -> String {
    ClaudeOutputRedactor::from_environment().text(text, max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_exact_values_before_bounding_text_and_nested_events() {
        let redactor = ClaudeOutputRedactor {
            secrets: vec!["private-value-8742".to_string()],
        };
        assert_eq!(
            redactor.text("before private-value-8742 after", 24),
            "before [REDACTED] after"
        );
        let event = redactor.event(serde_json::json!({
            "message": {"content": [{"type": "text", "text": "private-value-8742"}]}
        }));
        assert!(!event.to_string().contains("private-value-8742"));
        assert!(event.to_string().contains("[REDACTED]"));
    }
}
