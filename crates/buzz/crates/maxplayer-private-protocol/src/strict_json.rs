//! Reject duplicate keys recursively BEFORE serde can discard them in maps/optional objects.
use super::{Error, Result};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use std::{collections::HashSet, fmt};
struct Unique;
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("unique-key JSON")
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                while a.next_element::<Unique>()?.is_some() {}
                Ok(Unique)
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut a: A,
            ) -> std::result::Result<Unique, A::Error> {
                let mut keys = HashSet::new();
                while let Some(k) = a.next_key::<String>()? {
                    if !keys.insert(k) {
                        return Err(de::Error::custom("duplicate JSON key"));
                    }
                    a.next_value::<Unique>()?;
                }
                Ok(Unique)
            }
        }
        d.deserialize_any(V)
    }
}
pub fn validate(bytes: &[u8]) -> Result<()> {
    let mut d = serde_json::Deserializer::from_slice(bytes);
    Unique::deserialize(&mut d).map_err(|_| Error("invalid or duplicate-key JSON"))?;
    d.end().map_err(|_| Error("trailing JSON data"))
}
