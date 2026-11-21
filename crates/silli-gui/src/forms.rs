//! Form questions — an agent asking for values rather than permission. What the person has entered
//! so far, and the answer's values encoded as they are sent: a choice is its value, choices an
//! array of values, yes/no a bool, text a string, a number a number; an empty text or number
//! field is left out.
use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use silli_proto::sfp::{FormField, Question};

/// One field's entry.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub picked: BTreeSet<String>,
    pub text: String,
    pub on: bool,
}

/// A question's entries by field key.
pub type Form = BTreeMap<String, Entry>;

#[derive(Debug, Clone)]
pub enum Edit {
    /// A choice picked (one of a choice field; toggled in a choices field).
    Pick {
        key: String,
        value: String,
    },
    Text {
        key: String,
        text: String,
    },
    Toggle {
        key: String,
    },
}

/// The field kinds a form can carry; any other cannot be answered (docs/protocol.md).
const KINDS: [&str; 5] = ["choice", "choices", "text", "boolean", "number"];

pub fn answerable(question: &Question) -> bool {
    question.fields.iter().all(|f| KINDS.contains(&f.kind.as_str()))
}

pub fn title(field: &FormField) -> &str {
    if field.title.is_empty() { &field.key } else { &field.title }
}

pub fn apply(form: &mut Form, question: &Question, edit: Edit) {
    match edit {
        Edit::Pick { key, value } => {
            let single = question.fields.iter().any(|f| f.key == key && f.kind == "choice");
            let entry = form.entry(key).or_default();
            if single {
                entry.picked = BTreeSet::from([value]);
            } else if !entry.picked.remove(&value) {
                entry.picked.insert(value);
            }
        }
        Edit::Text { key, text } => form.entry(key).or_default().text = text,
        Edit::Toggle { key } => {
            let entry = form.entry(key).or_default();
            entry.on = !entry.on;
        }
    }
}

/// The answer's values, or which field cannot be sent as it stands.
pub fn values(form: &Form, question: &Question) -> Result<BTreeMap<String, Value>, String> {
    let mut values = BTreeMap::new();
    for field in &question.fields {
        let entry = form.get(&field.key).cloned().unwrap_or_default();
        let text = entry.text.trim();
        let value = match field.kind.as_str() {
            "choice" => entry.picked.iter().next().cloned().map(Value::String),
            "choices" => Some(Value::Array(field.choices.iter().filter(|c| entry.picked.contains(&c.value)).map(|c| Value::String(c.value.clone())).collect())),
            "boolean" => Some(Value::Bool(entry.on)),
            "text" => (!text.is_empty()).then(|| Value::String(text.to_string())),
            "number" if text.is_empty() => None,
            "number" => Some(
                text.parse::<f64>()
                    .ok()
                    .and_then(serde_json::Number::from_f64)
                    .map(Value::Number)
                    .ok_or_else(|| format!("{} is not a number", title(field)))?,
            ),
            other => {
                return Err(format!("{}: a {other} field cannot be answered here", title(field)));
            }
        };
        if let Some(value) = value {
            values.insert(field.key.clone(), value);
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn question() -> Question {
        serde_json::from_value(json!({
            "question_id":"q", "agent_generation_id":"g", "kind":"form", "title":"Deploy",
            "options":[], "state":"open",
            "fields":[
                {"key":"env", "title":"Environment", "description":"", "kind":"choice",
                 "choices":[{"value":"stage","title":"Staging","description":""},
                            {"value":"prod","title":"Production","description":""}]},
                {"key":"steps", "title":"Steps", "description":"", "kind":"choices",
                 "choices":[{"value":"a","title":"A","description":""},
                            {"value":"b","title":"B","description":""}]},
                {"key":"why", "title":"Why", "description":"", "kind":"text", "choices":[]},
                {"key":"n", "title":"Count", "description":"", "kind":"number", "choices":[]},
                {"key":"ok", "title":"Sure", "description":"", "kind":"boolean", "choices":[]}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn values_are_encoded_as_the_frontend_sends_them() {
        let q = question();
        let mut form = Form::new();
        for edit in [
            Edit::Pick { key: "env".into(), value: "stage".into() },
            Edit::Pick { key: "env".into(), value: "prod".into() },
            Edit::Pick { key: "steps".into(), value: "b".into() },
            Edit::Pick { key: "steps".into(), value: "a".into() },
            Edit::Pick { key: "steps".into(), value: "b".into() },
            Edit::Text { key: "why".into(), text: "  ship it ".into() },
            Edit::Toggle { key: "ok".into() },
        ] {
            apply(&mut form, &q, edit);
        }
        assert_eq!(json!(values(&form, &q).unwrap()), json!({"env":"prod", "steps":["a"], "why":"ship it", "ok":true}));
        apply(&mut form, &q, Edit::Text { key: "n".into(), text: "three".into() });
        assert_eq!(values(&form, &q).unwrap_err(), "Count is not a number");
        apply(&mut form, &q, Edit::Text { key: "n".into(), text: "3.5".into() });
        assert_eq!(values(&form, &q).unwrap()["n"], json!(3.5));
        assert!(answerable(&q));
    }
}
