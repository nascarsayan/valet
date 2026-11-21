//! Order-preserving JSON value, so a rewritten `threads.json` keeps the key
//! order of the file it was read from, exactly as `JSON.parse` +
//! `JSON.stringify` do in the Git Worktree Diff extension.
//!
//! `serde_json::Value` sorts object keys (no `preserve_order` feature here),
//! which would rewrite every thread on the first reply.

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, SerializeSeq, Serializer};
use serde_json::Number;
use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(Number),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn obj() -> J {
        J::Obj(Vec::new())
    }

    pub fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut J> {
        match self {
            J::Obj(m) => m.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// JS property assignment: an existing key keeps its position, a new key
    /// is appended. No-op on non-objects.
    pub fn set(&mut self, key: &str, val: J) {
        if let J::Obj(m) = self {
            match m.iter_mut().find(|(k, _)| k == key) {
                Some((_, v)) => *v = val,
                None => m.push((key.to_owned(), val)),
            }
        }
    }

    /// Builder form of [`J::set`].
    pub fn with(mut self, key: &str, val: J) -> J {
        self.set(key, val);
        self
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            J::Num(n) => n.as_f64(),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&Vec<J>> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_arr_mut(&mut self) -> Option<&mut Vec<J>> {
        match self {
            J::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// JS truthiness.
    pub fn truthy(&self) -> bool {
        match self {
            J::Null => false,
            J::Bool(b) => *b,
            J::Num(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
            J::Str(s) => !s.is_empty(),
            J::Arr(_) | J::Obj(_) => true,
        }
    }

    /// The value as a JS template literal would print it.
    pub fn js(&self) -> String {
        match self {
            J::Null => "null".into(),
            J::Bool(b) => b.to_string(),
            J::Num(n) => num_js(n),
            J::Str(s) => s.clone(),
            J::Arr(a) => a
                .iter()
                .map(|v| match v {
                    J::Null => String::new(),
                    v => v.js(),
                })
                .collect::<Vec<_>>()
                .join(","),
            J::Obj(_) => "[object Object]".into(),
        }
    }

    pub fn int(n: i64) -> J {
        J::Num(Number::from(n))
    }

    pub fn str(s: impl Into<String>) -> J {
        J::Str(s.into())
    }

    pub fn to_pretty(&self) -> String {
        serde_json::to_string_pretty(self).expect("J always serialises")
    }

    pub fn to_compact(&self) -> String {
        serde_json::to_string(self).expect("J always serialises")
    }
}

/// `opt.js()` for a possibly missing property (`undefined` in JS).
pub fn js_opt(v: Option<&J>) -> String {
    v.map_or_else(|| "undefined".into(), J::js)
}

fn num_js(n: &Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    let f = n.as_f64().unwrap_or(0.0);
    if f.fract() == 0.0 && f.abs() < 1e21 {
        format!("{f:.0}")
    } else {
        f.to_string()
    }
}

/// Keys JS objects order first: canonical array indices, ascending.
fn array_index(k: &str) -> Option<u32> {
    let n: u32 = k.parse().ok()?;
    (n != u32::MAX && n.to_string() == k).then_some(n)
}

impl Serialize for J {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            J::Null => s.serialize_unit(),
            J::Bool(b) => s.serialize_bool(*b),
            J::Num(n) => n.serialize(s),
            J::Str(v) => s.serialize_str(v),
            J::Arr(a) => {
                let mut seq = s.serialize_seq(Some(a.len()))?;
                for v in a {
                    seq.serialize_element(v)?;
                }
                seq.end()
            }
            J::Obj(m) => {
                let mut idx: Vec<_> = m
                    .iter()
                    .filter_map(|(k, v)| array_index(k).map(|n| (n, k, v)))
                    .collect();
                idx.sort_by_key(|(n, _, _)| *n);
                let mut map = s.serialize_map(Some(m.len()))?;
                for (_, k, v) in idx {
                    map.serialize_entry(k, v)?;
                }
                for (k, v) in m.iter().filter(|(k, _)| array_index(k).is_none()) {
                    map.serialize_entry(k, v)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for J {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<J, D::Error> {
        d.deserialize_any(JVisitor)
    }
}

struct JVisitor;

impl<'de> Visitor<'de> for JVisitor {
    type Value = J;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any JSON value")
    }
    fn visit_bool<E>(self, v: bool) -> Result<J, E> {
        Ok(J::Bool(v))
    }
    fn visit_i64<E>(self, v: i64) -> Result<J, E> {
        Ok(J::Num(v.into()))
    }
    fn visit_u64<E>(self, v: u64) -> Result<J, E> {
        Ok(J::Num(v.into()))
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<J, E> {
        // JS has one number type: `1.0` reads back as `1`.
        if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
            return Ok(J::Num((v as i64).into()));
        }
        Number::from_f64(v)
            .map(J::Num)
            .ok_or_else(|| E::custom("non-finite number"))
    }
    fn visit_str<E>(self, v: &str) -> Result<J, E> {
        Ok(J::Str(v.to_owned()))
    }
    fn visit_string<E>(self, v: String) -> Result<J, E> {
        Ok(J::Str(v))
    }
    fn visit_unit<E>(self) -> Result<J, E> {
        Ok(J::Null)
    }
    fn visit_none<E>(self) -> Result<J, E> {
        Ok(J::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<J, A::Error> {
        let mut v = Vec::new();
        while let Some(e) = seq.next_element()? {
            v.push(e);
        }
        Ok(J::Arr(v))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<J, A::Error> {
        let mut o = J::obj();
        while let Some((k, v)) = map.next_entry::<String, J>()? {
            o.set(&k, v);
        }
        Ok(o)
    }
}

pub fn parse(raw: &str) -> serde_json::Result<J> {
    serde_json::from_str(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_keeps_key_order() {
        let raw = r#"{"z":1,"a":[true,null,"x"],"m":{"y":2.5,"b":{}}}"#;
        assert_eq!(parse(raw).unwrap().to_compact(), raw);
    }

    #[test]
    fn duplicate_key_keeps_first_position_last_value() {
        assert_eq!(
            parse(r#"{"a":1,"b":2,"a":3}"#).unwrap().to_compact(),
            r#"{"a":3,"b":2}"#
        );
    }

    #[test]
    fn integer_keys_first_like_js() {
        assert_eq!(
            parse(r#"{"b":1,"10":2,"2":3,"01":4}"#)
                .unwrap()
                .to_compact(),
            r#"{"2":3,"10":2,"b":1,"01":4}"#
        );
    }

    #[test]
    fn integral_floats_print_like_js() {
        assert_eq!(
            parse("[1.0, 2.50, -0.0]").unwrap().to_compact(),
            "[1,2.5,0]"
        );
    }

    #[test]
    fn pretty_matches_json_stringify_indent() {
        let v = parse(r#"{"a":[],"b":{},"c":[1]}"#).unwrap();
        assert_eq!(
            v.to_pretty(),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1\n  ]\n}"
        );
    }

    #[test]
    fn set_appends_or_replaces() {
        let mut o = J::obj().with("a", J::int(1));
        o.set("b", J::int(2));
        o.set("a", J::str("x"));
        assert_eq!(o.to_compact(), r#"{"a":"x","b":2}"#);
    }

    #[test]
    fn js_strings_and_truthiness() {
        assert_eq!(J::int(3).js(), "3");
        assert_eq!(js_opt(None), "undefined");
        assert!(!J::str("").truthy());
        assert!(!J::int(0).truthy());
        assert!(J::Arr(vec![]).truthy());
    }
}
