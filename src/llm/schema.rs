//! JSON Schema for the curation response.
//!
//! Sent as `output_config.format.schema`, which constrains decoding so the
//! first text block is guaranteed to be a JSON document matching this shape.
//!
//! Schema subset rules the API enforces — violating any of these is a 400:
//!   * every object must set `additionalProperties: false`
//!   * every property must be listed in `required` (optionality is expressed
//!     with a nullable `anyOf`, not by omission)
//!   * no numeric bounds (`minimum`/`maximum`/`multipleOf`), no string bounds
//!     (`minLength`/`maxLength`), no recursion
//!
//! Because `maxItems` is unavailable, the track count is requested in prose in
//! the prompt and enforced by truncation after the fact.

use serde_json::{Value, json};

pub fn curation_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "playlist_title": {
                "type": "string",
                "description": "A short, evocative playlist name (2–5 words). No quotes, no emoji, no date."
            },
            "summary": {
                "type": "string",
                "description": "One paragraph (<= 300 characters) describing the through-line of the selection, addressed to the listener."
            },
            "tracks": {
                "type": "array",
                "description": "The recommended tracks, in the order they should appear in the playlist.",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "Exact track title as released. No featured-artist suffix, no remaster or edition annotation."
                        },
                        "artist": {
                            "type": "string",
                            "description": "Primary credited artist, spelled as they appear on streaming services."
                        },
                        "reason": {
                            "type": "string",
                            "description": "One sentence on why this track fits THIS listener and THIS brief. Reference something concrete from their profile."
                        },
                        "mood": {
                            "type": "string",
                            "description": "One or two words describing the track's emotional register, e.g. \"brooding\", \"propulsive\"."
                        },
                        "language": {
                            "type": "string",
                            "enum": ["en", "ru", "instrumental", "other"],
                            "description": "Predominant language of the lyrics."
                        },
                        "confidence": {
                            "type": "number",
                            "description": "0.0–1.0: how certain you are this exact track exists on Spotify under this exact title and artist spelling."
                        }
                    },
                    "required": ["title", "artist", "reason", "mood", "language", "confidence"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["playlist_title", "summary", "tracks"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk the schema and assert the API's structural constraints hold, so a
    /// future edit cannot silently introduce a 400.
    fn assert_valid(node: &Value) {
        if node.get("type").and_then(Value::as_str) == Some("object") {
            assert_eq!(
                node.get("additionalProperties"),
                Some(&json!(false)),
                "every object needs additionalProperties:false"
            );
            let props = node
                .get("properties")
                .and_then(Value::as_object)
                .expect("object needs properties");
            let required: Vec<&str> = node
                .get("required")
                .and_then(Value::as_array)
                .expect("object needs required")
                .iter()
                .filter_map(Value::as_str)
                .collect();
            for key in props.keys() {
                assert!(
                    required.contains(&key.as_str()),
                    "`{key}` is not in required"
                );
            }
            for value in props.values() {
                assert_valid(value);
            }
        }
        for banned in [
            "minimum",
            "maximum",
            "minLength",
            "maxLength",
            "minItems",
            "maxItems",
            "multipleOf",
        ] {
            assert!(node.get(banned).is_none(), "`{banned}` is not supported");
        }
        if let Some(items) = node.get("items") {
            assert_valid(items);
        }
    }

    #[test]
    fn schema_satisfies_api_constraints() {
        assert_valid(&curation_schema());
    }
}

/// Translate the canonical schema into Gemini's OpenAPI-subset dialect.
///
/// Gemini rejects `additionalProperties` outright, and ignores several JSON
/// Schema keywords. Everything it does understand — `type`, `description`,
/// `enum`, `items`, `properties`, `required` — is passed through unchanged, and
/// `propertyOrdering` is added so the model emits fields in a stable order
/// (Gemini's documented way to make structured output deterministic).
pub fn for_gemini(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                if key == "additionalProperties" {
                    continue;
                }
                out.insert(key.clone(), for_gemini(value));
            }
            // Preserve declaration order so the model fills fields predictably.
            if let Some(Value::Object(properties)) = map.get("properties") {
                let ordering: Vec<Value> = properties
                    .keys()
                    .map(|k| Value::String(k.clone()))
                    .collect();
                out.insert("propertyOrdering".into(), Value::Array(ordering));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(for_gemini).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod gemini_tests {
    use super::*;

    fn assert_no_additional_properties(node: &Value) {
        if let Value::Object(map) = node {
            assert!(
                !map.contains_key("additionalProperties"),
                "gemini rejects additionalProperties"
            );
            for value in map.values() {
                assert_no_additional_properties(value);
            }
        }
        if let Value::Array(items) = node {
            for item in items {
                assert_no_additional_properties(item);
            }
        }
    }

    #[test]
    fn strips_additional_properties_everywhere() {
        assert_no_additional_properties(&for_gemini(&curation_schema()));
    }

    #[test]
    fn keeps_required_and_enums() {
        let converted = for_gemini(&curation_schema());
        assert!(converted["required"].as_array().is_some());
        assert!(
            converted["properties"]["tracks"]["items"]["properties"]["language"]["enum"]
                .as_array()
                .is_some()
        );
    }

    #[test]
    fn adds_property_ordering() {
        let converted = for_gemini(&curation_schema());
        let ordering = converted["propertyOrdering"].as_array().expect("ordering");
        assert!(ordering.contains(&json!("tracks")));
    }
}
