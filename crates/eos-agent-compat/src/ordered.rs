//! JSON objects keep their keys sorted here, but Claude Code hashes an MCP
//! server's headers in the order the config wrote them (its sign-in's
//! storage key). Documents read for MCP configs note that order beside each
//! `headers` object as [`HEADER_ORDER`].

use std::fmt;

use serde::Deserialize;
use serde::de::{MapAccess, SeqAccess, Visitor};
use serde_json::Value;

/// The key holding a `headers` object's names in written order.
pub const HEADER_ORDER: &str = "__headerOrder";

/// A JSON tree that remembers object key order (values beyond structure are dropped).
enum Ordered {
    Object(Vec<(String, Ordered)>),
    Array(Vec<Ordered>),
    Other,
}

impl<'de> Deserialize<'de> for Ordered {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Ordered;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Ordered, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = map.next_entry::<String, Ordered>()? {
                    out.push((k, v));
                }
                Ok(Ordered::Object(out))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Ordered, A::Error> {
                let mut out = Vec::new();
                while let Some(v) = seq.next_element::<Ordered>()? {
                    out.push(v);
                }
                Ok(Ordered::Array(out))
            }
            fn visit_bool<E>(self, _: bool) -> Result<Ordered, E> {
                Ok(Ordered::Other)
            }
            fn visit_i64<E>(self, _: i64) -> Result<Ordered, E> {
                Ok(Ordered::Other)
            }
            fn visit_u64<E>(self, _: u64) -> Result<Ordered, E> {
                Ok(Ordered::Other)
            }
            fn visit_f64<E>(self, _: f64) -> Result<Ordered, E> {
                Ok(Ordered::Other)
            }
            fn visit_str<E>(self, _: &str) -> Result<Ordered, E> {
                Ok(Ordered::Other)
            }
            fn visit_unit<E>(self) -> Result<Ordered, E> {
                Ok(Ordered::Other)
            }
        }
        de.deserialize_any(V)
    }
}

/// `json` as a value, each `headers` object of two or more names noted
/// with their written order.
pub(crate) fn parse(json: &str) -> Option<Value> {
    let mut value: Value = serde_json::from_str(json).ok()?;
    if json.contains("\"headers\"")
        && let Ok(order) = serde_json::from_str::<Ordered>(json)
    {
        annotate(&mut value, &order);
    }
    Some(value)
}

fn annotate(value: &mut Value, order: &Ordered) {
    match (value, order) {
        (Value::Object(map), Ordered::Object(keys)) => {
            let mut noted = None;
            for (key, child) in keys {
                if key == "headers"
                    && let Ordered::Object(names) = child
                    && names.len() > 1
                {
                    noted = Some(names.iter().map(|(n, _)| Value::String(n.clone())).collect::<Vec<_>>());
                }
                if let Some(v) = map.get_mut(key) {
                    annotate(v, child);
                }
            }
            if let Some(names) = noted {
                map.insert(HEADER_ORDER.to_owned(), Value::Array(names));
            }
        }
        (Value::Array(items), Ordered::Array(orders)) => {
            for (v, o) in items.iter_mut().zip(orders) {
                annotate(v, o);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_the_written_order_of_headers() {
        let doc = parse(
            r#"{"mcpServers":{"a":{"type":"http","headers":{"Zeta":"1","alpha":"2"}},"b":{"headers":{"x":"1"}}}}"#,
        )
        .expect("json");
        assert_eq!(doc["mcpServers"]["a"][HEADER_ORDER], serde_json::json!(["Zeta", "alpha"]));
        assert!(doc["mcpServers"]["b"].get(HEADER_ORDER).is_none());
    }
}
