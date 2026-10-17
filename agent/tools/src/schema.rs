//! Tool definitions: the input JSON schema each tool advertises and the
//! validation of the arguments it is called with. Both follow what the
//! TypeScript version produced with zod 4 and the MCP SDK (same schema,
//! same key order, same error messages), so the model sees no difference.

use std::collections::HashMap;

use serde_json::{Map, Value, json};

use crate::limits::MAX_SAFE_INTEGER;

pub enum Kind {
    Str {
        min: Option<usize>,
        max: Option<usize>,
    },
    /// An integer; `exclusive` makes `min` a lower bound that is not allowed.
    Int {
        min: Option<i64>,
        exclusive: bool,
        max: Option<i64>,
    },
}

pub struct Field {
    pub name: &'static str,
    pub kind: Kind,
    pub required: bool,
    pub description: Option<&'static str>,
}

pub const fn string(
    name: &'static str,
    max: Option<usize>,
    description: Option<&'static str>,
) -> Field {
    Field {
        name,
        kind: Kind::Str { min: None, max },
        required: false,
        description,
    }
}

/// The optional `limit` every listing tool takes (1..=25).
pub const fn limit() -> Field {
    Field {
        name: "limit",
        kind: Kind::Int {
            min: Some(1),
            exclusive: false,
            max: Some(25),
        },
        required: false,
        description: None,
    }
}

pub struct Tool {
    pub name: &'static str,
    pub title: &'static str,
    pub description: String,
    pub fields: Vec<Field>,
}

/// Every tool is read-only; Hermes uses the hint to skip write approvals.
fn read_only() -> Value {
    json!({ "readOnlyHint": true, "destructiveHint": false, "openWorldHint": false })
}

impl Field {
    fn json_schema(&self) -> Value {
        let mut o = Map::new();
        // zod puts the description of an optional field first, that of a
        // required one last.
        if let (false, Some(d)) = (self.required, self.description) {
            o.insert("description".into(), d.into());
        }
        match self.kind {
            Kind::Str { min, max } => {
                o.insert("type".into(), "string".into());
                if let Some(min) = min {
                    o.insert("minLength".into(), min.into());
                }
                if let Some(max) = max {
                    o.insert("maxLength".into(), max.into());
                }
            }
            Kind::Int {
                min,
                exclusive,
                max,
            } => {
                o.insert("type".into(), "integer".into());
                let safe = MAX_SAFE_INTEGER as i64;
                match (min, exclusive) {
                    (Some(m), true) => o.insert("exclusiveMinimum".into(), m.into()),
                    (Some(m), false) => o.insert("minimum".into(), m.into()),
                    (None, _) => o.insert("minimum".into(), (-safe).into()),
                };
                o.insert("maximum".into(), max.unwrap_or(safe).into());
            }
        }
        if let (true, Some(d)) = (self.required, self.description) {
            o.insert("description".into(), d.into());
        }
        Value::Object(o)
    }
}

impl Tool {
    /// The entry in tools/list.
    pub fn definition(&self) -> Value {
        let properties: Map<String, Value> = self
            .fields
            .iter()
            .map(|f| (f.name.to_owned(), f.json_schema()))
            .collect();
        let mut schema = Map::new();
        schema.insert(
            "$schema".into(),
            "http://json-schema.org/draft-07/schema#".into(),
        );
        schema.insert("type".into(), "object".into());
        schema.insert("properties".into(), Value::Object(properties));
        let required: Vec<&str> = self
            .fields
            .iter()
            .filter(|f| f.required)
            .map(|f| f.name)
            .collect();
        if !required.is_empty() {
            schema.insert("required".into(), json!(required));
        }
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "inputSchema": Value::Object(schema),
            "annotations": read_only(),
            "execution": { "taskSupport": "forbidden" },
        })
    }

    /// Checks the arguments; unknown keys are ignored (zod strips them). The
    /// error is the text the MCP SDK put after "Invalid arguments for tool X: ".
    pub fn validate(&self, args: Option<&Value>) -> Result<Args, String> {
        let obj = match args {
            Some(Value::Object(o)) => o,
            other => {
                return Err(format!(
                    "Invalid input: expected object, received {}",
                    type_name(other)
                ));
            }
        };
        let mut issues = Vec::new();
        let mut out = Args::default();
        for f in &self.fields {
            let at = |m: String| format!("{m} at {}", f.name);
            let value = obj.get(f.name);
            match (&f.kind, value) {
                (_, None) if !f.required => {}
                (Kind::Str { min, max }, Some(Value::String(s))) => {
                    // zod counts code points.
                    let n = s.chars().count();
                    let before = issues.len();
                    if let Some(min) = min.filter(|m| n < *m) {
                        issues.push(at(format!(
                            "Too small: expected string to have >={min} characters"
                        )));
                    }
                    if let Some(max) = max.filter(|m| n > *m) {
                        issues.push(at(format!(
                            "Too big: expected string to have <={max} characters"
                        )));
                    }
                    if issues.len() == before {
                        out.strings.insert(f.name, s.clone());
                    }
                }
                (
                    Kind::Int {
                        min,
                        exclusive,
                        max,
                    },
                    Some(Value::Number(num)),
                ) => {
                    let n = num.as_f64().unwrap_or(f64::NAN);
                    if n.fract() != 0.0 || !n.is_finite() {
                        issues.push(at("Invalid input: expected int, received number".into()));
                        continue;
                    }
                    let before = issues.len();
                    if n > MAX_SAFE_INTEGER {
                        issues.push(at("Too big: expected int to be <=9007199254740991".into()));
                    }
                    if n < -MAX_SAFE_INTEGER {
                        issues.push(at(
                            "Too small: expected int to be >=-9007199254740991".into()
                        ));
                    }
                    if let Some(m) = *min {
                        if *exclusive && n <= m as f64 {
                            issues.push(at(format!("Too small: expected number to be >{m}")));
                        } else if !*exclusive && n < m as f64 {
                            issues.push(at(format!("Too small: expected number to be >={m}")));
                        }
                    }
                    if let Some(m) = max.filter(|m| n > *m as f64) {
                        issues.push(at(format!("Too big: expected number to be <={m}")));
                    }
                    if issues.len() == before {
                        // A safe integer, so the cast is exact.
                        out.ints.insert(f.name, n as i64);
                    }
                }
                (Kind::Str { .. }, v) => {
                    issues.push(at(format!(
                        "Invalid input: expected string, received {}",
                        type_name(v)
                    )));
                }
                (Kind::Int { .. }, v) => {
                    issues.push(at(format!(
                        "Invalid input: expected number, received {}",
                        type_name(v)
                    )));
                }
            }
        }
        if issues.is_empty() {
            Ok(out)
        } else {
            Err(issues.join("\n"))
        }
    }
}

/// zod's name for the type of a value it did not expect.
pub fn type_name(v: Option<&Value>) -> &'static str {
    match v {
        None => "undefined",
        Some(Value::Null) => "null",
        Some(Value::Bool(_)) => "boolean",
        Some(Value::Number(_)) => "number",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
    }
}

/// Validated arguments: only the fields the call gave.
#[derive(Default, Debug)]
pub struct Args {
    strings: HashMap<&'static str, String>,
    ints: HashMap<&'static str, i64>,
}

impl Args {
    pub fn str(&self, name: &str) -> Option<&str> {
        self.strings.get(name).map(String::as_str)
    }
    pub fn int(&self, name: &str) -> Option<i64> {
        self.ints.get(name).copied()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn tool() -> Tool {
        Tool {
            name: "t",
            title: "T",
            description: "d".into(),
            fields: vec![
                Field {
                    name: "q",
                    kind: Kind::Str {
                        min: Some(1),
                        max: Some(5),
                    },
                    required: true,
                    description: Some("Query"),
                },
                Field {
                    name: "id",
                    kind: Kind::Int {
                        min: Some(0),
                        exclusive: true,
                        max: None,
                    },
                    required: false,
                    description: None,
                },
                limit(),
            ],
        }
    }

    fn err(v: Value) -> String {
        tool().validate(Some(&v)).unwrap_err()
    }

    #[test]
    fn arguments_are_checked_like_zod() {
        let a = tool()
            .validate(Some(&json!({"q": "abc", "limit": 5.0, "extra": true})))
            .unwrap();
        assert_eq!(
            (a.str("q"), a.int("limit"), a.int("id")),
            (Some("abc"), Some(5), None)
        );
        assert_eq!(
            tool().validate(None).unwrap_err(),
            "Invalid input: expected object, received undefined"
        );
        assert_eq!(
            tool().validate(Some(&json!([]))).unwrap_err(),
            "Invalid input: expected object, received array"
        );
        assert_eq!(
            err(json!({})),
            "Invalid input: expected string, received undefined at q"
        );
        assert_eq!(
            err(json!({"q": ""})),
            "Too small: expected string to have >=1 characters at q"
        );
        assert_eq!(
            err(json!({"q": "abcdef"})),
            "Too big: expected string to have <=5 characters at q"
        );
        assert!(tool().validate(Some(&json!({"q": "🎉🎉🎉🎉🎉"}))).is_ok());
        assert_eq!(
            err(json!({"q": "🎉🎉🎉🎉🎉🎉"})),
            "Too big: expected string to have <=5 characters at q"
        );
        assert_eq!(
            err(json!({"q": "a", "limit": 2.5})),
            "Invalid input: expected int, received number at limit"
        );
        assert_eq!(
            err(json!({"q": "a", "limit": 0})),
            "Too small: expected number to be >=1 at limit"
        );
        assert_eq!(
            err(json!({"q": "a", "limit": 26})),
            "Too big: expected number to be <=25 at limit"
        );
        assert_eq!(
            err(json!({"q": "a", "limit": null})),
            "Invalid input: expected number, received null at limit"
        );
        assert_eq!(
            err(json!({"q": "a", "id": 0})),
            "Too small: expected number to be >0 at id"
        );
        assert_eq!(
            err(json!({"q": "a", "limit": 1e300})),
            "Too big: expected int to be <=9007199254740991 at limit\nToo big: expected number to be <=25 at limit"
        );
        assert_eq!(
            err(json!({"q": 5, "limit": "x"})),
            "Invalid input: expected string, received number at q\nInvalid input: expected number, received string at limit"
        );
    }

    #[test]
    fn schemas_have_zods_shape() {
        let d = tool().definition();
        assert_eq!(
            d["inputSchema"].to_string(),
            r#"{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","properties":{"q":{"type":"string","minLength":1,"maxLength":5,"description":"Query"},"id":{"type":"integer","exclusiveMinimum":0,"maximum":9007199254740991},"limit":{"type":"integer","minimum":1,"maximum":25}},"required":["q"]}"#
        );
        let opt = string("from", None, Some("From"));
        assert_eq!(
            opt.json_schema().to_string(),
            r#"{"description":"From","type":"string"}"#
        );
    }
}
