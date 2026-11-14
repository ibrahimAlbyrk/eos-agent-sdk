//! JSON that keeps object keys in file order, written back the way
//! `JSON.stringify(value, null, 1)` writes it, so a notebook edit only
//! changes the cell it touches.

use std::fmt;

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Number;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Json {
    Null,
    Bool(bool),
    Num(Number),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    pub(crate) fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Self::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Sets `key`, keeping its place when it already exists.
    pub(crate) fn set(&mut self, key: &str, value: Json) {
        if let Self::Obj(fields) = self {
            match fields.iter_mut().find(|(k, _)| k == key) {
                Some((_, v)) => *v = value,
                None => fields.push((key.to_owned(), value)),
            }
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(s) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Num(n) => n.as_u64(),
            _ => None,
        }
    }

    pub(crate) fn str(s: impl Into<String>) -> Self {
        Self::Str(s.into())
    }

    pub(crate) fn to_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        let newline = |out: &mut String, depth: usize| {
            out.push('\n');
            out.extend(std::iter::repeat_n(' ', depth));
        };
        match self {
            Self::Null => out.push_str("null"),
            Self::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Self::Num(n) => out.push_str(&js_number(n)),
            Self::Str(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
            Self::Arr(items) if items.is_empty() => out.push_str("[]"),
            Self::Obj(fields) if fields.is_empty() => out.push_str("{}"),
            Self::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, depth + 1);
                    item.write(out, depth + 1);
                }
                newline(out, depth);
                out.push(']');
            }
            Self::Obj(fields) => {
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    newline(out, depth + 1);
                    out.push_str(&serde_json::to_string(key).unwrap_or_default());
                    out.push_str(": ");
                    value.write(out, depth + 1);
                }
                newline(out, depth);
                out.push('}');
            }
        }
    }
}

/// Whole floats print without a fraction, as in JavaScript.
fn js_number(n: &Number) -> String {
    match n.as_f64() {
        Some(f) if !n.is_i64() && !n.is_u64() && f.fract() == 0.0 && f.abs() < 1e21 => format!("{f}"),
        _ => n.to_string(),
    }
}

impl<'de> Deserialize<'de> for Json {
    fn deserialize<D: Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        de.deserialize_any(JsonVisitor)
    }
}

struct JsonVisitor;

impl<'de> Visitor<'de> for JsonVisitor {
    type Value = Json;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Json, E> {
        Ok(Json::Null)
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Json, E> {
        Ok(Json::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Json, E> {
        Ok(Json::Num(v.into()))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Json, E> {
        Ok(Json::Num(v.into()))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Json, E> {
        Number::from_f64(v).map(Json::Num).ok_or_else(|| E::custom("not a finite number"))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Json, E> {
        Ok(Json::Str(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Json, E> {
        Ok(Json::Str(v))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Json, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Json::Arr(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Json, A::Error> {
        let mut fields: Vec<(String, Json)> = Vec::with_capacity(map.size_hint().unwrap_or(0));
        while let Some((key, value)) = map.next_entry::<String, Json>()? {
            match fields.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => *v = value,
                None => fields.push((key, value)),
            }
        }
        Ok(Json::Obj(fields))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_order_and_writes_like_json_stringify() {
        let src = r#"{"z":1,"a":[1,{"k":"v\n"}],"e":{},"l":[],"f":2.0,"g":0.5}"#;
        let v: Json = serde_json::from_str(src).expect("json");
        assert_eq!(
            v.to_pretty(),
            "{\n \"z\": 1,\n \"a\": [\n  1,\n  {\n   \"k\": \"v\\n\"\n  }\n ],\n \"e\": {},\n \"l\": [],\n \"f\": 2,\n \"g\": 0.5\n}"
        );
    }
}
