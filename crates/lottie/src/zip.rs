//! A minimal ZIP writer (stored entries, PKWARE APPNOTE 6.3 local headers + central directory)
//! for `.lottie` (dotLottie) archives.

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// Build a ZIP archive with the given (name, bytes) entries, uncompressed.
pub fn store(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut central = Vec::new();
    let put16 = |v: &mut Vec<u8>, x: u16| v.extend_from_slice(&x.to_le_bytes());
    let put32 = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_le_bytes());
    for (name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let n = name.as_bytes();
        // Local file header.
        put32(&mut out, 0x0403_4b50);
        put16(&mut out, 20); // version needed
        put16(&mut out, 0x0800); // flags: UTF-8 names
        put16(&mut out, 0); // stored
        put16(&mut out, 0); // time
        put16(&mut out, 0x21); // date (1980-01-01)
        put32(&mut out, crc);
        put32(&mut out, data.len() as u32);
        put32(&mut out, data.len() as u32);
        put16(&mut out, n.len() as u16);
        put16(&mut out, 0);
        out.extend_from_slice(n);
        out.extend_from_slice(data);
        // Central directory record.
        put32(&mut central, 0x0201_4b50);
        put16(&mut central, 20); // made by
        put16(&mut central, 20);
        put16(&mut central, 0x0800);
        put16(&mut central, 0);
        put16(&mut central, 0);
        put16(&mut central, 0x21);
        put32(&mut central, crc);
        put32(&mut central, data.len() as u32);
        put32(&mut central, data.len() as u32);
        put16(&mut central, n.len() as u16);
        put16(&mut central, 0); // extra
        put16(&mut central, 0); // comment
        put16(&mut central, 0); // disk
        put16(&mut central, 0); // internal attrs
        put32(&mut central, 0); // external attrs
        put32(&mut central, offset);
        central.extend_from_slice(n);
    }
    let cd_offset = out.len() as u32;
    let cd_len = central.len() as u32;
    out.extend_from_slice(&central);
    put32(&mut out, 0x0605_4b50);
    put16(&mut out, 0);
    put16(&mut out, 0);
    put16(&mut out, entries.len() as u16);
    put16(&mut out, entries.len() as u16);
    put32(&mut out, cd_len);
    put32(&mut out, cd_offset);
    put16(&mut out, 0);
    out
}

/// Whether an entry name is a relative path inside the archive: not absolute (`/…`, `\…`), no
/// drive (`C:…`) and no `..` component.
fn is_relative_name(name: &str) -> bool {
    let drive = name.as_bytes().get(1) == Some(&b':');
    !name.starts_with(['/', '\\']) && !drive && !name.split(['/', '\\']).any(|c| c == "..")
}

/// Read the stored (uncompressed) entries of a ZIP archive written by [`store`] or any other
/// writer that did not compress. Compressed entries and entries whose names are not relative
/// paths inside the archive are skipped.
pub fn read_stored(data: &[u8]) -> Vec<(String, Vec<u8>)> {
    read_stored_borrowed(data).into_iter().map(|(name, bytes)| (name, bytes.to_vec())).collect()
}

/// Read stored entries with payloads borrowed from the input archive.
pub(crate) fn read_stored_borrowed(data: &[u8]) -> Vec<(String, &[u8])> {
    let mut out = vec![];
    let mut i = 0usize;
    while let Some(name_start) = i.checked_add(30) {
        let Some(header) = data.get(i..name_start).and_then(|h| <&[u8; 30]>::try_from(h).ok()) else { break };
        if header.get(..4) != Some(b"PK\x03\x04") {
            break;
        }
        let method = u16::from_le_bytes([header[8], header[9]]);
        let size = u32::from_le_bytes([header[18], header[19], header[20], header[21]]) as usize;
        let nlen = u16::from_le_bytes([header[26], header[27]]) as usize;
        let xlen = u16::from_le_bytes([header[28], header[29]]) as usize;
        let Some(name_end) = name_start.checked_add(nlen) else { break };
        let Some(body) = name_end.checked_add(xlen) else { break };
        let Some(end) = body.checked_add(size) else { break };
        let (Some(name), Some(bytes)) = (data.get(name_start..name_end), data.get(body..end)) else { break };
        let name = String::from_utf8_lossy(name).to_string();
        if method == 0 && is_relative_name(&name) {
            out.push((name, bytes));
        }
        i = end;
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn stored_payloads_borrow_archive() {
        let archive = super::store(&[("a.json".into(), b"{}".to_vec()), ("images/unused.bin".into(), b"unused".to_vec())]);
        let entries = super::read_stored_borrowed(&archive);
        assert_eq!(entries, [("a.json".into(), b"{}".as_slice()), ("images/unused.bin".into(), b"unused".as_slice())]);
        let mut record = 0;
        for ((name, payload), expected) in entries.iter().zip([b"{}".as_slice(), b"unused".as_slice()]) {
            let body = record + 30 + name.len();
            assert_eq!(payload.len(), expected.len());
            assert!(std::ptr::eq(payload.as_ptr(), archive[body..].as_ptr()));
            record = body + expected.len();
        }
        assert_eq!(super::read_stored(&archive), [("a.json".into(), b"{}".to_vec()), ("images/unused.bin".into(), b"unused".to_vec())]);
    }

    #[test]
    fn crc_and_roundtrip() {
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
        let z = super::store(&[("a.json".into(), b"{}".to_vec()), ("b/c.txt".into(), b"hello".to_vec())]);
        let back = super::read_stored(&z);
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].0, "b/c.txt");
        assert_eq!(back[1].1, b"hello");
    }

    #[test]
    fn entries_outside_the_archive_root_are_skipped() {
        let names = ["../up.txt", "a/../../up.txt", "/abs/x", "\\abs\\x", "C:\\x", "c:x", "a\\..\\x", "ok/x.txt", "ok/..x"];
        let z = super::store(&names.iter().map(|n| (n.to_string(), b"x".to_vec())).collect::<Vec<_>>());
        let back: Vec<String> = super::read_stored(&z).into_iter().map(|(n, _)| n).collect();
        assert_eq!(back, ["ok/x.txt", "ok/..x"]);
    }
}
