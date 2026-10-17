// SPDX-License-Identifier: Apache-2.0
//! The version an executable reports (`VS_FIXEDFILEINFO.dwFileVersion`), read without wine and
//! without loading the file: headers, the section table, and the RT_VERSION resource only, by
//! seeking. Premiere's exe is 800+ MB; this reads a few KB of it.
//!
//! This is what Windows shows as "File version" in the file's properties, and the binary that
//! will actually run. For most Adobe apps it equals the build Adobe records (Photoshop.exe
//! 27.8.0.13 = hdpim.db BuildVersion 27.8.0.13); Lightroom Classic's build number
//! (15.6.0.202609251514) does not fit the 16-bit fields, so callers treat it as corroboration.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

fn rd(f: &mut File, off: u64, n: usize) -> Option<Vec<u8>> {
    f.seek(SeekFrom::Start(off)).ok()?;
    let mut b = vec![0u8; n];
    f.read_exact(&mut b).ok()?;
    Some(b)
}

fn u16le(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32le(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// `[major, minor, build, revision]` of the file version, or None if the file is not a PE or
/// carries no version resource.
pub fn file_version(path: &Path) -> Option<[u16; 4]> {
    let mut f = File::open(path).ok()?;
    let dos = rd(&mut f, 0, 64)?;
    if &dos[0..2] != b"MZ" {
        return None;
    }
    let pe_off = u32le(&dos, 0x3c)? as u64;
    let hdr = rd(&mut f, pe_off, 24)?;
    if &hdr[0..4] != b"PE\0\0" {
        return None;
    }
    let nsect = u16le(&hdr, 6)? as usize;
    let opt_size = u16le(&hdr, 20)? as usize;
    let opt = rd(&mut f, pe_off + 24, opt_size)?;
    let magic = u16le(&opt, 0)?;
    // Data directory 2 = resources. PE32: directories at 96; PE32+: at 112.
    let dd = match magic {
        0x10b => 96,
        0x20b => 112,
        _ => return None,
    };
    let rsrc_rva = u32le(&opt, dd + 2 * 8)?;
    if rsrc_rva == 0 {
        return None;
    }
    let sects = rd(&mut f, pe_off + 24 + opt_size as u64, nsect * 40)?;
    let to_off = |rva: u32| -> Option<u64> {
        for i in 0..nsect {
            let s = &sects[i * 40..i * 40 + 40];
            let vsize = u32le(s, 8)?;
            let va = u32le(s, 12)?;
            let rawsize = u32le(s, 16)?;
            let raw = u32le(s, 20)?;
            let span = vsize.max(rawsize);
            if rva >= va && rva < va + span {
                return Some((rva - va + raw) as u64);
            }
        }
        None
    };
    let base = to_off(rsrc_rva)?;
    // Walk: type (RT_VERSION = 16) -> first name -> first language -> data entry.
    let find = |f: &mut File, dir_off: u64, want_id: Option<u32>| -> Option<u32> {
        let d = rd(f, dir_off, 16)?;
        let named = u16le(&d, 12)? as u64;
        let ids = u16le(&d, 14)? as u64;
        let entries = rd(f, dir_off + 16, ((named + ids) * 8) as usize)?;
        for i in 0..(named + ids) as usize {
            let name = u32le(&entries, i * 8)?;
            let off = u32le(&entries, i * 8 + 4)?;
            match want_id {
                Some(w) if name & 0x8000_0000 == 0 && name == w => return Some(off),
                None => return Some(off),
                _ => {}
            }
        }
        None
    };
    let t = find(&mut f, base, Some(16))?;
    if t & 0x8000_0000 == 0 {
        return None;
    }
    let n = find(&mut f, base + (t & 0x7fff_ffff) as u64, None)?;
    if n & 0x8000_0000 == 0 {
        return None;
    }
    let l = find(&mut f, base + (n & 0x7fff_ffff) as u64, None)?;
    if l & 0x8000_0000 != 0 {
        return None;
    }
    let data = rd(&mut f, base + l as u64, 16)?;
    let data_rva = u32le(&data, 0)?;
    let size = (u32le(&data, 4)? as usize).min(64 * 1024);
    let blob = rd(&mut f, to_off(data_rva)?, size)?;
    // VS_FIXEDFILEINFO starts with its signature, 0xFEEF04BD, dword-aligned after the key.
    let sig = 0xFEEF04BDu32.to_le_bytes();
    let at = blob.windows(4).position(|w| w == sig)?;
    let ms = u32le(&blob, at + 8)?;
    let ls = u32le(&blob, at + 12)?;
    Some([(ms >> 16) as u16, (ms & 0xffff) as u16, (ls >> 16) as u16, (ls & 0xffff) as u16])
}

/// The file version as Adobe writes builds: "27.8.0.13".
pub fn file_version_string(path: &Path) -> Option<String> {
    file_version(path).map(|v| format!("{}.{}.{}.{}", v[0], v[1], v[2], v[3]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PE32 with one section holding a resource tree RT_VERSION/1/1033 -> a
    /// VS_VERSIONINFO blob carrying VS_FIXEDFILEINFO 27.8.0.13.
    fn tiny_pe() -> Vec<u8> {
        let mut f = vec![0u8; 0x400];
        f[0] = b'M';
        f[1] = b'Z';
        f[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        f[0x80..0x84].copy_from_slice(b"PE\0\0");
        f[0x86..0x88].copy_from_slice(&1u16.to_le_bytes()); // one section
        f[0x94..0x96].copy_from_slice(&0xE0u16.to_le_bytes()); // optional header size
        let opt = 0x98;
        f[opt..opt + 2].copy_from_slice(&0x10bu16.to_le_bytes());
        // resource directory RVA 0x1000
        f[opt + 96 + 16..opt + 96 + 20].copy_from_slice(&0x1000u32.to_le_bytes());
        let sec = opt + 0xE0;
        f[sec..sec + 5].copy_from_slice(b".rsrc");
        f[sec + 8..sec + 12].copy_from_slice(&0x200u32.to_le_bytes()); // vsize
        f[sec + 12..sec + 16].copy_from_slice(&0x1000u32.to_le_bytes()); // va
        f[sec + 16..sec + 20].copy_from_slice(&0x200u32.to_le_bytes()); // raw size
        f[sec + 20..sec + 24].copy_from_slice(&0x200u32.to_le_bytes()); // raw ptr
        let r = 0x200;
        let dir = |f: &mut Vec<u8>, at: usize, id: u32, off: u32| {
            f[at + 14..at + 16].copy_from_slice(&1u16.to_le_bytes());
            f[at + 16..at + 20].copy_from_slice(&id.to_le_bytes());
            f[at + 20..at + 24].copy_from_slice(&off.to_le_bytes());
        };
        dir(&mut f, r, 16, 0x8000_0018);
        dir(&mut f, r + 0x18, 1, 0x8000_0030);
        dir(&mut f, r + 0x30, 1033, 0x48);
        // data entry at r+0x48: RVA of the blob (0x1060), size 0x60
        f[r + 0x48..r + 0x4c].copy_from_slice(&0x1060u32.to_le_bytes());
        f[r + 0x4c..r + 0x50].copy_from_slice(&0x60u32.to_le_bytes());
        let b = r + 0x60 + 0x28; // somewhere inside the blob, dword aligned
        f[b..b + 4].copy_from_slice(&0xFEEF04BDu32.to_le_bytes());
        f[b + 8..b + 12].copy_from_slice(&((27u32 << 16) | 8).to_le_bytes());
        f[b + 12..b + 16].copy_from_slice(&13u32.to_le_bytes());
        f
    }

    #[test]
    fn reads_the_fixed_file_version() {
        let p = std::env::temp_dir().join(format!("mudhut-pe-{}.exe", std::process::id()));
        std::fs::write(&p, tiny_pe()).unwrap();
        assert_eq!(file_version_string(&p).as_deref(), Some("27.8.0.13"));
        std::fs::write(&p, b"MZ not really a PE").unwrap();
        assert_eq!(file_version(&p), None);
        let _ = std::fs::remove_file(&p);
    }
}
