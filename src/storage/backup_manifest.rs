//! Read the physical manifest's byte binding and recovery ranges.
use super::{Result, durable};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, Read},
    path::Path,
};

#[derive(Deserialize)]
struct Manifest {
    #[serde(rename = "WAL-Ranges")]
    ranges: Value,
}

struct HashedReader {
    file: File,
    hash: Sha256,
}

impl Read for HashedReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.file.read(buffer)?;
        self.hash.update(&buffer[..count]);
        Ok(count)
    }
}

pub(super) fn read(path: &Path) -> Result<(Value, String)> {
    let mut reader = HashedReader {
        file: durable::open_regular(path, false)?,
        hash: Sha256::new(),
    };
    // Deserialize only the recovery ranges. Serde validates but discards the
    // per-file inventory, so its size does not inflate memory or hit carrier limits.
    // from_reader also checks EOF, binding all bytes including trailing whitespace.
    let manifest: Manifest = serde_json::from_reader(BufReader::new(&mut reader))?;
    Ok((
        json!({"WAL-Ranges":manifest.ranges}),
        format!("{:x}", reader.hash.finalize()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::codec;
    use serde_json::json;
    use std::{fs, io::Write};

    #[test]
    fn large_file_inventory_preserves_complete_byte_digest_and_wal_ranges() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("backup_manifest");
        let ranges = json!([{"Timeline":1,"Start-LSN":"0/100","End-LSN":"0/200"}]);
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(b"{\"Files\":[").unwrap();
        let entry = json!({"Path":"p".repeat(600),"Size":8192,"Checksum-Algorithm":"SHA256","Checksum":"a".repeat(64)}).to_string();
        for index in 0..32768 {
            if index != 0 {
                file.write_all(b",").unwrap();
            }
            file.write_all(entry.as_bytes()).unwrap();
        }
        writeln!(
            file,
            "],\"WAL-Ranges\":{ranges},\"Manifest-Checksum\":\"{}\"}}",
            "b".repeat(64)
        )
        .unwrap();
        drop(file);
        assert!(fs::metadata(&path).unwrap().len() > 16 << 20);
        let (manifest, digest) = read(&path).unwrap();
        assert_eq!(manifest["WAL-Ranges"], ranges);
        assert_eq!(digest, codec::digest(&fs::read(&path).unwrap()));
    }

    #[test]
    fn rejects_malformed_inventory_and_trailing_bytes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("backup_manifest");
        for bytes in [
            b"{\"Files\":[invalid],\"WAL-Ranges\":[]}".as_slice(),
            b"{\"WAL-Ranges\":[]} trailing",
            b"{\"WAL-Ranges\":[]}{\"WAL-Ranges\":[]}",
        ] {
            fs::write(&path, bytes).unwrap();
            assert!(read(&path).is_err());
        }
    }
}
