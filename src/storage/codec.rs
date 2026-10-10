//! Byte-compatible codecs for existing Python JSON hash contracts.
use super::{Result, invalid};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fmt::Write, fs::File, io::Read, path::Path};

/// Decode containers structurally rather than through Value's private serde
/// protocol. With arbitrary_precision, its synthetic number-map discriminator
/// otherwise also recognizes a literal JSON object with that field name.
pub fn decode(bytes: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("JSON is not UTF-8"))?;
    decode_str(text)
}

pub fn decode_str(text: &str) -> Result<Value> {
    // Validate the entire token stream first, retaining serde_json's nesting
    // bound and rejecting trailing data before any application sees a value.
    depth_bound(text.as_bytes())?;
    serde_json::from_str::<serde::de::IgnoredAny>(text)?;
    value(text.trim())
}

fn depth_bound(bytes: &[u8]) -> Result<()> {
    // IgnoredAny skips allocation and does not enforce Value's recursion bound.
    // Count containers before its validating walk, ignoring escaped string data.
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
                        return Err(invalid("JSON nesting limit exceeded"));
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    Ok(())
}

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
            .ok_or_else(|| invalid("invalid JSON container"))
    }

    fn raw(&mut self) -> Result<&'a str> {
        let mut stream =
            serde_json::Deserializer::from_str(self.0).into_iter::<serde::de::IgnoredAny>();
        stream
            .next()
            .ok_or_else(|| invalid("missing JSON value"))??;
        let (raw, rest) = self.0.split_at(stream.byte_offset());
        self.0 = rest;
        Ok(raw.trim())
    }
}

fn value(text: &str) -> Result<Value> {
    let mut cursor = Cursor(text);
    if cursor.eat(b'{') {
        let mut members = std::collections::BTreeMap::new();
        if !cursor.eat(b'}') {
            loop {
                let key: String = serde_json::from_str(cursor.raw()?)?;
                cursor.expect(b':')?;
                // Match Python's last decoded member, including escaped keys.
                members.insert(key, cursor.raw()?);
                if cursor.eat(b'}') {
                    break;
                }
                cursor.expect(b',')?;
            }
        }
        let fields = members
            .into_iter()
            .map(|(key, raw)| Ok((key, value(raw)?)))
            .collect::<Result<serde_json::Map<String, Value>>>()?;
        Ok(Value::Object(fields))
    } else if cursor.eat(b'[') {
        let mut items = Vec::new();
        if !cursor.eat(b']') {
            loop {
                items.push(value(cursor.raw()?)?);
                if cursor.eat(b']') {
                    break;
                }
                cursor.expect(b',')?;
            }
        }
        Ok(Value::Array(items))
    } else {
        // Only scalar JSON tokens reach Value's decoder, so no literal object
        // can collide with its number discriminator. Large integers stay exact.
        Ok(serde_json::from_str(text)?)
    }
}

pub fn encode(value: &Value, compact: bool) -> Result<Vec<u8>> {
    let mut output = String::new();
    append(value, compact, &mut output)?;
    Ok(output.into_bytes())
}

fn quoted(value: &str, output: &mut String) -> Result<()> {
    let encoded = serde_json::to_string(value)?;
    for character in encoded.chars() {
        if character.is_ascii() && character != '\u{7f}' {
            output.push(character);
        } else {
            let mut units = [0; 2];
            for unit in character.encode_utf16(&mut units) {
                write!(output, "\\u{unit:04x}")
                    .map_err(|_| invalid("JSON serialization failed"))?;
            }
        }
    }
    Ok(())
}

fn append(value: &Value, compact: bool, output: &mut String) -> Result<()> {
    let comma = if compact { "," } else { ", " };
    match value {
        Value::Null => output.push_str("null"),
        Value::Bool(value) => output.push_str(if *value { "true" } else { "false" }),
        Value::String(value) => quoted(value, output)?,
        Value::Number(value) => {
            let spelling = value.to_string();
            if spelling.contains(['.', 'e', 'E']) {
                let number = value
                    .as_f64()
                    .ok_or_else(|| invalid("invalid JSON number"))?;
                if !number.is_finite() {
                    return Err(invalid("non-finite JSON number"));
                }
                if number.abs() >= 1e16 || (number != 0.0 && number.abs() < 1e-4) {
                    let text = format!("{number:e}");
                    let (mantissa, exponent) = text
                        .split_once('e')
                        .ok_or_else(|| invalid("invalid exponent"))?;
                    let exponent: i32 =
                        exponent.parse().map_err(|_| invalid("invalid exponent"))?;
                    write!(output, "{mantissa}e{exponent:+03}")
                        .map_err(|_| invalid("JSON serialization failed"))?;
                } else if number.fract() == 0.0 {
                    write!(output, "{number:.1}")
                        .map_err(|_| invalid("JSON serialization failed"))?;
                } else {
                    write!(output, "{number}").map_err(|_| invalid("JSON serialization failed"))?;
                }
            } else {
                output.push_str(if spelling == "-0" { "0" } else { &spelling });
            }
        }
        Value::Array(values) => {
            output.push('[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push_str(comma);
                }
                append(value, compact, output)?;
            }
            output.push(']');
        }
        Value::Object(values) => {
            output.push('{');
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort();
            for (index, key) in keys.into_iter().enumerate() {
                if index != 0 {
                    output.push_str(comma);
                }
                quoted(key, output)?;
                output.push_str(if compact { ":" } else { ": " });
                append(&values[key], compact, output)?;
            }
            output.push('}');
        }
    }
    Ok(())
}

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn file_digest(path: &Path) -> Result<String> {
    let mut file = super::durable::open_regular(path, false)?;
    hash_reader(&mut file)
}

pub fn hash_reader(file: &mut File) -> Result<String> {
    let mut hash = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
