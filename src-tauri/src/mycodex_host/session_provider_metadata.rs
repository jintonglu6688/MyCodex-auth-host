//! Change only owned provider tokens, preserving every record's byte offset.
use super::Result;
use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::{value::RawValue, Value};
use std::collections::BTreeMap;
use std::fmt;

pub(super) fn legacy(id: &str) -> bool {
    id.strip_prefix("mycodex_resp_")
        .is_some_and(|suffix| suffix.len() == 12 && suffix.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(super) fn source(text: &str, thread: &str) -> Result<String> {
    let mut selected = None;
    let mut owner = thread.to_string();
    for (index, line) in text.lines().enumerate() {
        let value: Value = serde_json::from_str(line).map_err(|_| "provider_history_invalid")?;
        let raw = if index == 0 {
            if value["type"] != "session_meta" || value["payload"]["id"] != thread {
                return Err("provider_history_invalid");
            }
            Some(field(line, &["payload", "model_provider"])?)
        } else if value["type"] == "event_msg"
            && value["payload"]["type"] == "thread_settings_applied"
        {
            if let Some(id) = value["payload"]["thread_id"].as_str() {
                owner = id.to_string();
            }
            if owner == thread {
                Some(field(
                    line,
                    &["payload", "thread_settings", "model_provider_id"],
                )?)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(raw) = raw {
            let provider = serde_json::from_str::<String>(raw.get())
                .map_err(|_| "provider_history_invalid")?;
            // A newer custom setting must not hide an older header still needing migration.
            if selected.is_none() || provider != "custom" {
                selected = Some(provider);
            }
        }
    }
    selected.ok_or("provider_history_invalid")
}

pub(super) fn rewrite(text: &str, thread: &str, source: &str) -> Result<String> {
    let mut output = String::with_capacity(text.len());
    let mut owner = thread.to_string();
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let value: Value = serde_json::from_str(line).map_err(|_| "provider_history_invalid")?;
        let path: &[&str] = if index == 0 {
            if value["type"] != "session_meta" || value["payload"]["id"] != thread {
                return Err("provider_history_invalid");
            }
            &["payload", "model_provider"]
        } else if value["type"] == "event_msg"
            && value["payload"]["type"] == "thread_settings_applied"
        {
            if let Some(id) = value["payload"]["thread_id"].as_str() {
                owner = id.to_string();
            }
            if owner != thread {
                output.push_str(line);
                continue;
            }
            &["payload", "thread_settings", "model_provider_id"]
        } else {
            output.push_str(line);
            continue;
        };
        let raw = field(line, path)?;
        let provider: String =
            serde_json::from_str(raw.get()).map_err(|_| "provider_history_invalid")?;
        if provider == "custom" {
            output.push_str(line);
            continue;
        }
        if provider != source && !legacy(&provider) {
            return Err("provider_history_conflict");
        }
        let replacement = "\"custom\"";
        if raw.get().len() < replacement.len() {
            return Err("provider_history_invalid");
        }
        let offset = raw.get().as_ptr() as usize - line.as_ptr() as usize;
        output.push_str(&line[..offset]);
        output.push_str(replacement);
        output.extend(std::iter::repeat_n(
            ' ',
            raw.get().len() - replacement.len(),
        ));
        output.push_str(&line[offset + raw.get().len()..]);
    }
    if output.is_empty() {
        return Err("provider_history_invalid");
    }
    Ok(output)
}

fn field<'a>(text: &'a str, path: &[&str]) -> Result<&'a RawValue> {
    let object: Object<'a> = serde_json::from_str(text).map_err(|_| "provider_history_invalid")?;
    let raw = *object.0.get(path[0]).ok_or("provider_history_invalid")?;
    if path.len() == 1 {
        Ok(raw)
    } else {
        field(raw.get(), &path[1..])
    }
}

// Duplicate keys are ambiguous even when serde_json's ordinary Value accepts them.
struct Object<'a>(BTreeMap<String, &'a RawValue>);
impl<'de> Deserialize<'de> for Object<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct ObjectVisitor;
        impl<'de> Visitor<'de> for ObjectVisitor {
            type Value = Object<'de>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("an object without duplicate keys")
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut fields = BTreeMap::new();
                while let Some((key, value)) = map.next_entry::<String, &'de RawValue>()? {
                    if fields.insert(key, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate key"));
                    }
                }
                Ok(Object(fields))
            }
        }
        deserializer.deserialize_map(ObjectVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const OLD: &str = "mycodex_resp_0123456789ab";

    #[test]
    fn preserves_bytes_offsets_foreign_settings_and_text() {
        let head = format!("{{ \"type\":\"session_meta\",\"payload\":{{\"id\":\"a\",\"model_provider\":\"{OLD}\"}} }}\r\n");
        let foreign = format!(
            "{}\n",
            json!({"type":"event_msg","payload":{"type":"thread_settings_applied","thread_id":"b","thread_settings":{"model_provider_id":OLD}}})
        );
        let message = format!(
            "{}\n",
            json!({"type":"response_item","payload":{"text":OLD,"encrypted_content":"opaque"}})
        );
        let original = format!("{head}{foreign}{message}");
        let result = rewrite(&original, "a", OLD).unwrap();
        assert_eq!(result.len(), original.len());
        assert_eq!(&result[head.len()..], &original[head.len()..]);
        assert!(result.contains("\"model_provider\":\"custom\""));
        assert_eq!(rewrite(&result, "a", OLD).unwrap(), result);
    }

    #[test]
    fn rejects_ambiguous_metadata() {
        let duplicate = format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"a\",\"model_provider\":\"{OLD}\",\"model_provider\":\"{OLD}\"}}}}\n");
        assert!(rewrite(&duplicate, "a", OLD).is_err());
        assert!(!legacy("other_provider"));
        assert!(!legacy("mycodex_resp_not-an-id"));
    }

    #[test]
    fn finds_partially_migrated_metadata_in_either_order() {
        for (head, setting) in [(OLD, "custom"), ("custom", OLD)] {
            let text = format!(
                "{}\n{}\n",
                json!({"type":"session_meta","payload":{"id":"a","model_provider":head}}),
                json!({"type":"event_msg","payload":{"type":"thread_settings_applied","thread_id":"a","thread_settings":{"model_provider_id":setting}}})
            );
            assert_eq!(source(&text, "a").unwrap(), OLD);
            let result = rewrite(&text, "a", OLD).unwrap();
            assert_eq!(source(&result, "a").unwrap(), "custom");
            assert_eq!(result.len(), text.len());
        }
    }
}
