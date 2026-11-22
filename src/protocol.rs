use crate::{
    memory::{format, io, shared, Binding, Store, WriteOutcome},
    Result,
};
use base64::Engine;
use serde_json::{json, Value};
use std::io::Read;

struct Unique(Value);
impl<'de> serde::Deserialize<'de> for Unique {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = Unique;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON value with unique keys")
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_string<E: serde::de::Error>(
                self,
                v: String,
            ) -> std::result::Result<Unique, E> {
                Ok(Unique(json!(v)))
            }
            fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut values = Vec::new();
                while let Some(Unique(v)) = a.next_element()? {
                    values.push(v)
                }
                Ok(Unique(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some((k, Unique(v))) = a.next_entry::<String, Unique>()? {
                    if values.insert(k, v).is_some() {
                        return Err(serde::de::Error::custom("Duplicate JSON fields"));
                    }
                }
                Ok(Unique(Value::Object(values)))
            }
        }
        d.deserialize_any(Visitor)
    }
}
pub fn read_json(reader: impl Read, limit: usize) -> Result<Value> {
    let mut data = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut data)
        .map_err(io::err)?;
    if data.len() > limit {
        return Err("Memory input exceeds the size limit".into());
    }
    let data = data.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&data);
    let Unique(v) = serde_json::from_slice(data).map_err(|_| "Invalid UTF-8 JSON memory input")?;
    if !v.is_object() {
        return Err("Expected a JSON object".into());
    }
    Ok(v)
}
pub fn action(v: &Value) -> Result<Value> {
    let m = v.as_object().ok_or("Invalid staged memory action")?;
    let shared = match m.get("scope") {
        None => false,
        Some(scope) if scope == "shared" => true,
        _ => return Err("Invalid staged memory scope".into()),
    };
    let arity = 2 + usize::from(shared);
    let mut request = match v["action"].as_str() {
        Some("skip") if m.len() == 1 => return Ok(json!({"action":"skip"})),
        Some(a @ ("remember" | "draft")) if m.len() == arity && m.contains_key("candidate") => {
            let fields = format::candidate(&v["candidate"])?;
            if shared {
                shared::check_kind(&fields)?;
            }
            json!({"action":a,"candidate":fields})
        }
        Some(a @ ("approve" | "forget")) if m.len() == arity && m.contains_key("id") => {
            json!({"action":a,"id":format::id(v["id"].as_str().ok_or("Invalid staged memory action")?)?})
        }
        _ => return Err("Invalid staged memory action".into()),
    };
    if shared {
        request["scope"] = json!("shared");
    }
    Ok(request)
}
pub fn encode(v: &Value) -> Result<String> {
    let text = v.to_string();
    if text.len() > 20000 {
        return Err("Memory action exceeds the size limit".into());
    }
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text))
}
pub fn decode(payload: &str) -> Result<Value> {
    action(&decode_json(payload)?)
}
pub fn decode_json(payload: &str) -> Result<Value> {
    if payload.len() > 26667 || !format::matches(r"\A[A-Za-z0-9_-]+\z", payload) {
        return Err("Invalid encoded memory action".into());
    }
    let data = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(io::err)?;
    if base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&data) != payload {
        return Err("Invalid encoded memory action".into());
    }
    read_json(data.as_slice(), 20000)
}

pub fn apply_action(
    store: &Store,
    binding: &Binding,
    data: &Value,
    session: Option<&str>,
    turn: Option<&str>,
) -> Result<Value> {
    let request = action(data)?;
    if request["action"] == "skip" {
        return Ok(json!({"phase": "skipped"}));
    }
    let shared;
    let binding = if request["scope"] == "shared" {
        shared = shared::for_action(store, binding, &request)?;
        &shared
    } else {
        binding
    };
    let outcome = match request["action"].as_str().unwrap() {
        "approve" => store.approve_outcome(binding, request["id"].as_str().unwrap())?,
        "forget" => WriteOutcome::Written(store.forget(binding, request["id"].as_str().unwrap())?),
        action => store.save_outcome(
            binding,
            &request["candidate"],
            action == "remember",
            session,
            turn,
        )?,
    };
    Ok(match outcome {
        WriteOutcome::Written(mut record) => {
            if binding.shared {
                record["scope"] = json!("shared");
            }
            json!({"phase": "applied", "record": record})
        }
        WriteOutcome::Forgotten => json!({"phase": "discarded_forgotten"}),
    })
}
