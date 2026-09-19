//! ISO9660 validation and conservative architecture evidence. No filename is called detection.
use anyhow::{bail, ensure, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub(crate) struct Inspection {
    pub detected: Option<String>,
    pub hint: Option<String>,
    pub label: String,
}
fn sector(file: &mut File, lba: u32) -> Result<[u8; 2048]> {
    file.seek(SeekFrom::Start(u64::from(lba) * 2048))?;
    let mut data = [0; 2048];
    file.read_exact(&mut data)?;
    Ok(data)
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}
#[derive(Clone)]
struct Entry {
    extent: u32,
    length: u32,
    directory: bool,
}
fn entry(rec: &[u8]) -> Result<Entry> {
    ensure!(rec.len() >= 34, "truncated ISO directory record");
    Ok(Entry {
        extent: le32(&rec[2..]),
        length: le32(&rec[10..]),
        directory: rec[25] & 2 != 0,
    })
}
fn child(file: &mut File, dir: &Entry, name: &str, total: u64) -> Result<Option<Entry>> {
    ensure!(
        dir.directory && dir.length <= 16 * 1024 * 1024,
        "invalid/oversized ISO directory"
    );
    let start = u64::from(dir.extent) * 2048;
    ensure!(
        start + u64::from(dir.length) <= total,
        "ISO directory extent exceeds image"
    );
    let mut data = vec![0; dir.length as usize];
    file.seek(SeekFrom::Start(start))?;
    file.read_exact(&mut data)?;
    let mut pos = 0;
    while pos < data.len() {
        let size = data[pos] as usize;
        if size == 0 {
            pos = (pos / 2048 + 1) * 2048;
            continue;
        }
        ensure!(
            size >= 34 && pos + size <= data.len(),
            "invalid ISO directory record length"
        );
        let rec = &data[pos..pos + size];
        let n = rec[32] as usize;
        ensure!(33 + n <= rec.len(), "invalid ISO directory filename length");
        let raw = String::from_utf8_lossy(&rec[33..33 + n]);
        if raw
            .split(';')
            .next()
            .unwrap_or("")
            .eq_ignore_ascii_case(name)
        {
            return Ok(Some(entry(rec)?));
        }
        pos += size;
    }
    Ok(None)
}
fn pe_arch(file: &mut File, e: &Entry, total: u64) -> Result<Option<String>> {
    let start = u64::from(e.extent) * 2048;
    ensure!(
        start + u64::from(e.length) <= total,
        "EFI file extent exceeds ISO"
    );
    if e.directory || e.length < 64 {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(start))?;
    let mut dos = [0; 64];
    file.read_exact(&mut dos)?;
    if &dos[..2] != b"MZ" {
        return Ok(None);
    }
    let offset = u64::from(le32(&dos[60..]));
    if offset + 6 > u64::from(e.length) {
        return Ok(None);
    }
    file.seek(SeekFrom::Start(start + offset))?;
    let mut pe = [0; 6];
    file.read_exact(&mut pe)?;
    if &pe[..4] != b"PE\0\0" {
        return Ok(None);
    }
    Ok(match u16::from_le_bytes([pe[4], pe[5]]) {
        0x8664 => Some("x86_64".into()),
        0xaa64 => Some("aarch64".into()),
        _ => None,
    })
}
pub(crate) fn inspect(path: &Path) -> Result<Inspection> {
    let mut file = File::open(path).context("open installer ISO")?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file(),
        "installer must be a regular ISO file, not a physical device"
    );
    ensure!(
        meta.len() >= 18 * 2048,
        "installer is too small to be an ISO9660 image"
    );
    let mut pvd = None;
    let mut terminated = false;
    // A boot descriptor may precede the PVD; don't assume sector 16 is type 1.
    for lba in 16..512.min((meta.len() / 2048) as u32) {
        let d = sector(&mut file, lba)?;
        if &d[1..6] != b"CD001" || d[6] != 1 {
            break;
        }
        if d[0] == 1 && pvd.is_none() {
            pvd = Some(d);
        }
        if d[0] == 255 {
            terminated = true;
            break;
        }
    }
    let pvd = pvd.context("no ISO9660 CD001 primary volume descriptor; UDF-only images are unsupported; select an Ubuntu installation ISO")?;
    ensure!(
        terminated,
        "ISO9660 volume descriptor sequence has no valid terminator"
    );
    ensure!(
        u16::from_le_bytes([pvd[128], pvd[129]]) == 2048,
        "unsupported ISO logical block size"
    );
    let label = String::from_utf8_lossy(&pvd[40..72]).trim().to_owned();
    let root = entry(&pvd[156..190])?;
    let mut detected = None;
    if let Some(efi) = child(&mut file, &root, "EFI", meta.len())? {
        if let Some(boot) = child(&mut file, &efi, "BOOT", meta.len())? {
            for name in ["BOOTX64.EFI", "BOOTAA64.EFI"] {
                if let Some(e) = child(&mut file, &boot, name, meta.len())? {
                    if let Some(a) = pe_arch(&mut file, &e, meta.len())? {
                        if let Some(old) = &detected {
                            if old != &a {
                                bail!("multi-architecture EFI ISO is ambiguous; use a single-architecture installer");
                            }
                        }
                        detected = Some(a);
                    }
                }
            }
        }
    }
    let hint_text = format!(
        "{} {}",
        label,
        path.file_name().unwrap_or_default().to_string_lossy()
    )
    .to_ascii_lowercase();
    let x86 = ["amd64", "x86_64", "x64"]
        .iter()
        .any(|v| hint_text.contains(v));
    let arm = ["arm64", "aarch64"].iter().any(|v| hint_text.contains(v));
    let hint = match (x86, arm) {
        (true, false) => Some("x86_64".into()),
        (false, true) => Some("aarch64".into()),
        _ => None,
    };
    Ok(Inspection {
        detected,
        hint,
        label,
    })
}
