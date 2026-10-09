//! Project validated retention fields without decoding unused surrogate strings.
//! Original manifest bytes remain untouched; this is not a receipt/hash codec.
use super::{Result, durable, invalid};
use serde::{
    Deserialize, Deserializer,
    de::{self, IgnoredAny, Visitor},
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

/// Token boundaries come from serde_json's validating stream decoder, without
/// enabling raw_value's literal-object coercion in shared Value readers.
struct Cursor<'a>(&'a str);

impl<'a> Cursor<'a> {
    fn eat(&mut self, byte: u8) -> bool {
        self.0 = self.0.trim_start();
        if self.0.as_bytes().first() == Some(&byte) {
            self.0 = &self.0[1..];
            true
        } else {
            false
        }
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        self.eat(byte)
            .then_some(())
            .ok_or_else(|| invalid("invalid retention container"))
    }

    fn raw(&mut self) -> Result<&'a str> {
        let mut stream = serde_json::Deserializer::from_str(self.0).into_iter::<IgnoredAny>();
        stream
            .next()
            .ok_or_else(|| invalid("missing retention value"))??;
        let (raw, rest) = self.0.split_at(stream.byte_offset());
        self.0 = rest;
        Ok(raw.trim())
    }

    fn end(self) -> Result<()> {
        self.0
            .trim()
            .is_empty()
            .then_some(())
            .ok_or_else(|| invalid("trailing retention data"))
    }
}

struct Object<'a>(BTreeMap<Vec<u8>, &'a str>);

impl<'a> Object<'a> {
    fn parse(text: &'a str) -> Result<Self> {
        let mut cursor = Cursor(text);
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

fn depth_bound(bytes: &[u8]) -> Result<()> {
    // IgnoredAny validates the whole JSON token stream, including ignored values,
    // without recursive Value allocation. Preserve Value's existing default
    // nesting bound as well; quotes and escaped quotes do not count as structure.
    let (mut depth, mut string, mut escape) = (0usize, false, false);
    for byte in bytes {
        if string {
            if escape {
                escape = false;
            } else if *byte == b'\\' {
                escape = true;
            } else if *byte == b'"' {
                string = false;
            }
        } else {
            match byte {
                b'"' => string = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth >= 128 {
                        return Err(invalid("retention JSON nesting limit exceeded"));
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}

pub(super) fn read(path: &Path) -> Result<Value> {
    // FIFO manifests retain the approved prompt conservative-refusal contract.
    let mut bytes = Vec::new();
    durable::open_regular(path, false)?.read_to_end(&mut bytes)?;
    let text =
        std::str::from_utf8(&bytes).map_err(|_| invalid("retention manifest is not UTF-8"))?;
    depth_bound(&bytes)?;
    serde_json::from_str::<IgnoredAny>(text)?;
    let object = Object::parse(text)?;
    let mut ranges = Cursor(object.field(b"WAL-Ranges")?);
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
