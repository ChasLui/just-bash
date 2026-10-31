//! A small, dependency-free `tar` (ustar) reader/writer plus the gzip container
//! needed by `-z`.
//!
//! The crate is deliberately filesystem-free: callers hand over the entries to
//! write and receive the entries they read, so the shell keeps owning all path
//! resolution inside the in-memory filesystem.
//!
//! Compression uses *stored* deflate blocks. That is a valid gzip stream (the
//! fixtures only ever read back archives this crate wrote), and it keeps the
//! implementation dependency-free and deterministic.

/// One archive member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Member name as stored in the archive (directories keep a trailing `/`).
    pub path: String,
    pub data: Vec<u8>,
    pub is_dir: bool,
}

const BLOCK: usize = 512;

/// Build an uncompressed ustar archive.
pub fn archive(entries: &[Entry]) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in entries {
        out.extend_from_slice(&header(entry));
        if !entry.is_dir {
            out.extend_from_slice(&entry.data);
            let padding = (BLOCK - entry.data.len() % BLOCK) % BLOCK;
            out.extend(std::iter::repeat_n(0u8, padding));
        }
    }
    // Two zero blocks terminate the archive.
    out.extend(std::iter::repeat_n(0u8, BLOCK * 2));
    out
}

/// Parse an uncompressed ustar archive.
pub fn extract(bytes: &[u8]) -> Result<Vec<Entry>, String> {
    let mut entries = Vec::new();
    let mut offset = 0usize;
    while offset + BLOCK <= bytes.len() {
        let block = &bytes[offset..offset + BLOCK];
        if block.iter().all(|byte| *byte == 0) {
            break;
        }
        let name = read_string(&block[0..100]);
        if name.is_empty() {
            break;
        }
        let size = read_octal(&block[124..136]) as usize;
        let typeflag = block[156];
        let prefix = read_string(&block[345..500]);
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let is_dir = typeflag == b'5' || path.ends_with('/');
        offset += BLOCK;
        let data = if is_dir {
            Vec::new()
        } else {
            let end = (offset + size).min(bytes.len());
            let slice = bytes[offset..end].to_vec();
            offset += size + (BLOCK - size % BLOCK) % BLOCK;
            slice
        };
        entries.push(Entry {
            path: path.trim_end_matches('/').to_string(),
            data,
            is_dir,
        });
    }
    Ok(entries)
}

/// The member names an archive contains, in stored order.
pub fn list(bytes: &[u8]) -> Result<Vec<String>, String> {
    Ok(extract(bytes)?
        .into_iter()
        .map(|entry| {
            if entry.is_dir {
                format!("{}/", entry.path)
            } else {
                entry.path
            }
        })
        .collect())
}

fn header(entry: &Entry) -> [u8; BLOCK] {
    let mut block = [0u8; BLOCK];
    let name = if entry.is_dir {
        format!("{}/", entry.path.trim_end_matches('/'))
    } else {
        entry.path.clone()
    };
    let (prefix, name) = split_name(&name);
    write_string(&mut block[0..100], &name);
    write_octal(&mut block[100..108], 0o644);
    write_octal(&mut block[108..116], 0);
    write_octal(&mut block[116..124], 0);
    write_octal(&mut block[124..136], entry.data.len() as u64);
    write_octal(&mut block[136..148], 0);
    // Checksum is computed over the header with this field blanked.
    for byte in &mut block[148..156] {
        *byte = b' ';
    }
    block[156] = if entry.is_dir { b'5' } else { b'0' };
    write_string(&mut block[257..263], "ustar");
    block[263] = b'0';
    block[264] = b'0';
    write_string(&mut block[265..297], "root");
    write_string(&mut block[297..329], "root");
    write_string(&mut block[345..500], &prefix);
    let checksum: u64 = block.iter().map(|byte| *byte as u64).sum();
    write_checksum(&mut block[148..156], checksum);
    block
}

/// Split a long name into the ustar `prefix` and `name` fields.
fn split_name(name: &str) -> (String, String) {
    if name.len() <= 100 {
        return (String::new(), name.to_string());
    }
    // Prefer splitting at a `/` so `prefix` fits in 155 bytes.
    let bytes = name.as_bytes();
    for index in (0..bytes.len()).rev() {
        if bytes[index] == b'/' && index <= 155 {
            return (name[..index].to_string(), name[index + 1..].to_string());
        }
    }
    (String::new(), name.chars().take(100).collect())
}

fn read_string(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

fn write_string(field: &mut [u8], text: &str) {
    let bytes = text.as_bytes();
    let count = bytes.len().min(field.len());
    field[..count].copy_from_slice(&bytes[..count]);
}

fn read_octal(bytes: &[u8]) -> u64 {
    let text = read_string(bytes);
    u64::from_str_radix(text.trim(), 8).unwrap_or(0)
}

/// Octal fields are NUL-terminated, except the checksum which ends with a space.
fn write_octal(field: &mut [u8], value: u64) {
    let text = format!("{:0width$o}", value, width = field.len() - 1);
    let bytes = text.as_bytes();
    let count = bytes.len().min(field.len() - 1);
    field[..count].copy_from_slice(&bytes[bytes.len() - count..]);
}

fn write_checksum(field: &mut [u8], value: u64) {
    let text = format!("{:06o}", value);
    field[..6].copy_from_slice(text.as_bytes());
    field[6] = 0;
    field[7] = b' ';
}

/// Encode arbitrary bytes as a string that survives the in-memory filesystem.
///
/// The filesystem stores `String`, so binary archive bytes must be mapped
/// losslessly: every byte becomes the character with the same code point
/// (Latin-1). [`decode_bytes`] reverses this exactly.
pub fn encode_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| *byte as char).collect()
}

/// Reverse [`encode_bytes`].
pub fn decode_bytes(text: &str) -> Vec<u8> {
    text.chars().map(|ch| ch as u32 as u8).collect()
}

// ── gzip ────────────────────────────────────────────────────────────────────

const GZIP_HEADER: [u8; 10] = [0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 3];

/// Wrap `data` in a gzip container using stored deflate blocks.
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = GZIP_HEADER.to_vec();
    for chunk in data.chunks(65535) {
        let last = chunk.len() == data.chunks(65535).last().map_or(0, <[u8]>::len);
        out.push(if last { 1 } else { 0 });
        let len = chunk.len() as u16;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&(!len).to_le_bytes());
        out.extend_from_slice(chunk);
    }
    if data.is_empty() {
        out.push(1);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0xffffu16.to_le_bytes());
    }
    out.extend_from_slice(&crc32(data).to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

/// Unwrap a gzip container produced by [`gzip`].
pub fn gunzip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if bytes.len() < 18 || bytes[0] != 0x1f || bytes[1] != 0x8b {
        return Err("not in gzip format".to_string());
    }
    let mut offset = 10usize;
    let mut out = Vec::new();
    loop {
        let Some(&header) = bytes.get(offset) else {
            return Err("truncated gzip stream".to_string());
        };
        offset += 1;
        let last = header & 1 == 1;
        let kind = header >> 1 & 0b11;
        if kind != 0 {
            return Err("compressed gzip blocks are not supported".to_string());
        }
        // Skip to the next byte boundary, then read LEN/NLEN.
        let Some(len_bytes) = bytes.get(offset..offset + 4) else {
            return Err("truncated gzip block".to_string());
        };
        let len = u16::from_le_bytes([len_bytes[0], len_bytes[1]]) as usize;
        offset += 4;
        if offset + len > bytes.len() {
            return Err("truncated gzip block".to_string());
        }
        out.extend_from_slice(&bytes[offset..offset + len]);
        offset += len;
        if last {
            break;
        }
    }
    Ok(out)
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, contents: &str) -> Entry {
        Entry {
            path: path.to_string(),
            data: contents.as_bytes().to_vec(),
            is_dir: false,
        }
    }

    fn dir(path: &str) -> Entry {
        Entry {
            path: path.to_string(),
            data: Vec::new(),
            is_dir: true,
        }
    }

    #[test]
    fn round_trips_files_and_directories() {
        let entries = vec![
            dir("mydir"),
            file("mydir/file1.txt", "Content 1\n"),
            file("mydir/file2.txt", "Content 2\n"),
        ];
        let bytes = archive(&entries);
        let read = extract(&bytes).expect("extract");
        assert_eq!(read, entries);
        assert_eq!(
            list(&bytes).expect("list"),
            vec!["mydir/", "mydir/file1.txt", "mydir/file2.txt"]
        );
    }

    #[test]
    fn handles_names_needing_a_prefix() {
        let long = "a".repeat(120) + "/file.txt";
        let entries = vec![file(&long, "x")];
        let bytes = archive(&entries);
        let read = extract(&bytes).expect("extract");
        assert_eq!(read[0].path, long);
        assert_eq!(read[0].data, b"x");
    }

    #[test]
    fn gzip_round_trips() {
        let payload = vec![7u8; 200_000];
        let wrapped = gzip(&payload);
        assert_eq!(gunzip(&wrapped).expect("gunzip"), payload);
        // The container ends with the CRC and the uncompressed length.
        let tail = &wrapped[wrapped.len() - 8..];
        assert_eq!(
            u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]),
            crc32(&payload)
        );
    }

    #[test]
    fn gzip_round_trips_the_empty_input() {
        let wrapped = gzip(b"");
        assert_eq!(gunzip(&wrapped).expect("gunzip"), Vec::<u8>::new());
    }

    #[test]
    fn byte_encoding_is_lossless() {
        let raw: Vec<u8> = (0u8..=255).collect();
        let text = encode_bytes(&raw);
        assert_eq!(decode_bytes(&text), raw);
        // A gzip container must survive the same round trip.
        let wrapped = gzip(b"payload");
        assert_eq!(decode_bytes(&encode_bytes(&wrapped)), wrapped);
    }

    #[test]
    fn gzip_rejects_plain_data() {
        assert!(gunzip(b"not gzip at all").is_err());
    }
}
