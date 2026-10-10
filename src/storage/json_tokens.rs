//! Shared token boundaries and nesting admission; callers choose string decoding.
use super::{Result, invalid};
use serde::de::IgnoredAny;

pub(super) fn validate(text: &str, label: &str) -> Result<()> {
    // IgnoredAny validates syntax without enforcing Value's recursion bound.
    // Count containers first, ignoring quotes and escaped string contents.
    let (mut depth, mut string, mut escape) = (0usize, false, false);
    for byte in text.bytes() {
        if string {
            if escape {
                escape = false;
            } else if byte == b'\\' {
                escape = true;
            } else if byte == b'"' {
                string = false;
            }
        } else {
            match byte {
                b'"' => string = true,
                b'[' | b'{' => {
                    depth += 1;
                    if depth >= 128 {
                        return Err(invalid(format!("{label} nesting limit exceeded")));
                    }
                }
                b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    serde_json::from_str::<IgnoredAny>(text)?;
    Ok(())
}

pub(super) struct Cursor<'a> {
    remaining: &'a str,
    label: &'static str,
}

impl<'a> Cursor<'a> {
    pub(super) fn new(text: &'a str, label: &'static str) -> Self {
        Self {
            remaining: text,
            label,
        }
    }

    pub(super) fn eat(&mut self, byte: u8) -> bool {
        self.remaining = self.remaining.trim_start();
        if self.remaining.as_bytes().first() == Some(&byte) {
            self.remaining = &self.remaining[1..];
            true
        } else {
            false
        }
    }

    pub(super) fn expect(&mut self, byte: u8) -> Result<()> {
        self.eat(byte)
            .then_some(())
            .ok_or_else(|| invalid(format!("invalid {} container", self.label)))
    }

    pub(super) fn raw(&mut self) -> Result<&'a str> {
        let mut stream =
            serde_json::Deserializer::from_str(self.remaining).into_iter::<IgnoredAny>();
        stream
            .next()
            .ok_or_else(|| invalid(format!("missing {} value", self.label)))??;
        let (raw, rest) = self.remaining.split_at(stream.byte_offset());
        self.remaining = rest;
        Ok(raw.trim())
    }

    pub(super) fn end(self) -> Result<()> {
        self.remaining
            .trim()
            .is_empty()
            .then_some(())
            .ok_or_else(|| invalid(format!("trailing {} data", self.label)))
    }
}
