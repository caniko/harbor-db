//! Project validated retention fields without decoding unused surrogate strings.
//! Original manifest bytes remain untouched; this is not a receipt/hash codec.
use super::{
    Result, durable, invalid,
    json_tokens::{self, Cursor},
};
use serde::{
    Deserialize, Deserializer,
    de::{self, Visitor},
};
use serde_json::Value;
use std::{collections::BTreeMap, fmt, io::Read, path::Path};

struct Key(Vec<u8>);

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct Bytes;
        impl<'de> Visitor<'de> for Bytes {
            type Value = Key;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a JSON object key")
            }
            fn visit_bytes<E: de::Error>(self, bytes: &[u8]) -> std::result::Result<Key, E> {
                Ok(Key(bytes.to_vec()))
            }
        }
        // serde_json's byte-string decoder preserves escaped lone surrogates
        // and still resolves ordinary escapes used in required field names.
        deserializer.deserialize_bytes(Bytes)
    }
}

struct Object<'a>(BTreeMap<Vec<u8>, &'a str>);

impl<'a> Object<'a> {
    fn parse(text: &'a str) -> Result<Self> {
        let mut cursor = Cursor::new(text, "retention");
        cursor.expect(b'{')?;
        let mut fields = BTreeMap::new();
        if !cursor.eat(b'}') {
            loop {
                let Key(key) = serde_json::from_str(cursor.raw()?)?;
                cursor.expect(b':')?;
                let value = cursor.raw()?;
                if [
                    b"WAL-Ranges".as_slice(),
                    b"Timeline",
                    b"Start-LSN",
                    b"End-LSN",
                ]
                .contains(&key.as_slice())
                {
                    // Earlier invalid field types may be replaced. Decode only
                    // the last member for each resolved required key.
                    fields.insert(key, value);
                }
                if cursor.eat(b'}') {
                    break;
                }
                cursor.expect(b',')?;
            }
        }
        cursor.end()?;
        Ok(Self(fields))
    }

    fn field(&self, name: &[u8]) -> Result<&str> {
        self.0
            .get(name)
            .copied()
            .ok_or_else(|| invalid("missing retention field"))
    }
}

pub(super) fn read(path: &Path) -> Result<Value> {
    // FIFO manifests retain the approved prompt conservative-refusal contract.
    let mut bytes = Vec::new();
    durable::open_regular(path, false)?.read_to_end(&mut bytes)?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid("retention manifest is not UTF-8"))?;
    json_tokens::validate(text, "retention JSON")?;
    let object = Object::parse(text)?;
    let mut ranges = Cursor::new(object.field(b"WAL-Ranges")?, "retention");
    ranges.expect(b'[')?;
    let mut projected = Vec::new();
    if !ranges.eat(b']') {
        loop {
            let range = Object::parse(ranges.raw()?)?;
            // Typed decoding retains scalar admission even if a dependency
            // introduces additional Value sentinel conversions in the future.
            let timeline: u64 = serde_json::from_str(range.field(b"Timeline")?)?;
            let start: String = serde_json::from_str(range.field(b"Start-LSN")?)?;
            let end: String = serde_json::from_str(range.field(b"End-LSN")?)?;
            projected.push(
                serde_json::json!({"Timeline": timeline, "Start-LSN": start, "End-LSN": end}),
            );
            if ranges.eat(b']') {
                break;
            }
            ranges.expect(b',')?;
        }
    }
    ranges.end()?;
    Ok(serde_json::json!({"WAL-Ranges": projected}))
}
