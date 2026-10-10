//! Versioned bounded JSON-lines transport, separate from worker diagnostic streams.
use crate::storage::{Result, invalid};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

pub const VERSION: u32 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Execute {
        node: String,
        argv: Vec<String>,
        timeout_seconds: u64,
    },
    Start {
        node: String,
        allow_reboot: bool,
    },
    Stop {
        node: String,
    },
    Crash {
        node: String,
    },
    Reboot {
        node: String,
    },
    CopyTo {
        node: String,
        source: String,
        destination: String,
    },
    CopyFrom {
        node: String,
        source: String,
        destination: String,
    },
}

impl Request {
    pub fn execute(node: impl Into<String>, argv: Vec<String>) -> Self {
        Self::Execute {
            node: node.into(),
            argv,
            timeout_seconds: 60,
        }
    }
    fn validate(&self) -> Result<()> {
        let node = match self {
            Self::Execute {
                node,
                argv,
                timeout_seconds,
            } => {
                if argv.is_empty()
                    || argv.iter().any(|s| s.contains('\0'))
                    || argv[0].is_empty()
                    || *timeout_seconds == 0
                {
                    return Err(invalid("invalid bridge execution request"));
                }
                node
            }
            Self::Start { node, .. }
            | Self::Stop { node }
            | Self::Crash { node }
            | Self::Reboot { node } => node,
            Self::CopyTo {
                node,
                source,
                destination,
            }
            | Self::CopyFrom {
                node,
                source,
                destination,
            } => {
                if [source, destination]
                    .iter()
                    .any(|s| s.is_empty() || s.contains('\0'))
                {
                    return Err(invalid("invalid bridge transfer"));
                }
                node
            }
        };
        if node.is_empty() || node.contains('\0') {
            return Err(invalid("invalid bridge node"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub version: u32,
    pub id: u64,
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeError {
    pub kind: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u32,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<BridgeError>,
}

pub struct Client {
    stream: BufReader<UnixStream>,
    timeout: Duration,
    sequence: u64,
    poisoned: bool,
}

impl Client {
    pub fn new(stream: UnixStream, timeout: Duration) -> Result<Self> {
        if timeout.is_zero() {
            return Err(invalid("bridge timeout must be positive"));
        }
        stream.set_write_timeout(Some(timeout))?;
        Ok(Self {
            stream: BufReader::new(stream),
            timeout,
            sequence: 0,
            poisoned: false,
        })
    }

    pub fn call(&mut self, request: Request) -> Result<Value> {
        if self.poisoned {
            return Err(invalid(
                "bridge transport requires reconnection after a failed frame",
            ));
        }
        request.validate()?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("bridge sequence exhausted"))?;
        let bytes = serde_json::to_vec(&Envelope {
            version: VERSION,
            id: self.sequence,
            request,
        })?;
        if bytes.len() >= MAX_FRAME_BYTES {
            return Err(invalid("bridge request exceeds frame limit"));
        }
        let result = self.exchange(bytes);
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn exchange(&mut self, mut request: Vec<u8>) -> Result<Value> {
        request.push(b'\n');
        let began = Instant::now();
        self.stream.get_mut().write_all(&request)?;
        self.stream.get_mut().flush()?;
        let mut frame = Vec::new();
        loop {
            let remaining = self
                .timeout
                .checked_sub(began.elapsed())
                .filter(|d| !d.is_zero())
                .ok_or_else(|| invalid("bridge response timed out"))?;
            self.stream.get_mut().set_read_timeout(Some(remaining))?;
            let chunk = self.stream.fill_buf()?;
            if chunk.is_empty() {
                return Err(invalid("bridge control pipe closed before its response"));
            }
            let count = chunk
                .iter()
                .position(|b| *b == b'\n')
                .map_or(chunk.len(), |n| n + 1);
            if frame.len().saturating_add(count) > MAX_FRAME_BYTES {
                return Err(invalid("bridge response exceeds frame limit"));
            }
            let done = chunk[count - 1] == b'\n';
            frame.extend_from_slice(&chunk[..count]);
            self.stream.consume(count);
            if done {
                break;
            }
        }
        let response: Response = serde_json::from_slice(&frame)?;
        if response.version != VERSION || response.id != self.sequence {
            return Err(invalid("bridge response version or correlation mismatch"));
        }
        match (response.result, response.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) if !error.kind.is_empty() => {
                Err(invalid(format!("bridge {}: {}", error.kind, error.message)))
            }
            _ => Err(invalid(
                "bridge response must contain exactly one result or error",
            )),
        }
    }
}
