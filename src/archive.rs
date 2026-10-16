//! Opening an archive: header, password, last complete footer, index.

use crate::crypto::{read_password, Cipher};
use crate::format::*;
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Arc;

pub struct Opened {
    pub file: File,
    #[allow(dead_code)]
    pub header: Header,
    pub cipher: Option<Arc<Cipher>>,
    pub index: Index,
    /// End of the last complete footer. Anything after it is a partial, interrupted update.
    pub end: u64,
}

pub const ROLE_BLOCK: &[u8] = b"blk";
pub const ROLE_INDEX: &[u8] = b"idx";

fn read_at(f: &mut File, off: u64, len: usize) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(off))?;
    let mut b = vec![0u8; len];
    f.read_exact(&mut b)?;
    Ok(b)
}

/// Read and authenticate the index whose footer ends at `footer_end`.
fn index_at(f: &mut File, footer_end: u64, cipher: Option<&Cipher>) -> Result<Index> {
    if footer_end < HEADER_LEN + FOOTER_LEN {
        bail!("no footer");
    }
    let foot = read_at(f, footer_end - FOOTER_LEN, FOOTER_LEN as usize)?;
    if &foot[24..32] != END_MAGIC {
        bail!("no footer");
    }
    let ioff = u64::from_le_bytes(foot[0..8].try_into()?);
    let ilen = u64::from_le_bytes(foot[8..16].try_into()?);
    let iraw = u64::from_le_bytes(foot[16..24].try_into()?);
    if ioff < HEADER_LEN || ioff.checked_add(ilen).is_none_or(|e| e != footer_end - FOOTER_LEN) || iraw > 1 << 32 {
        bail!("corrupt footer");
    }
    let blob = read_at(f, ioff, ilen as usize)?;
    let comp = match cipher {
        Some(c) => c.open(ROLE_INDEX, ioff, &blob).context("wrong password, or the archive is damaged")?,
        None => blob,
    };
    // Stream-decode with a hard limit instead of trusting the recorded size up front.
    let mut raw = Vec::with_capacity((iraw as usize).min(comp.len().saturating_mul(32)).min(64 << 20));
    {
        use std::io::Read as _;
        let d = zstd::stream::read::Decoder::new(&comp[..]).context("corrupt index")?;
        d.take(iraw + 1).read_to_end(&mut raw).context("corrupt index")?;
    }
    if raw.len() as u64 != iraw {
        bail!("corrupt index (length mismatch)");
    }
    let idx = Index::decode(&raw)?;
    for b in &idx.blocks {
        if b.offset < HEADER_LEN || b.offset.checked_add(b.comp_len).is_none_or(|e| e > ioff) {
            bail!("block out of bounds");
        }
    }
    Ok(idx)
}

/// Open an archive. `ask` supplies the password if the archive is encrypted.
pub fn open(path: &Path) -> Result<Opened> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len();
    let mut hb = vec![0u8; HEADER_LEN as usize];
    file.read_exact(&mut hb).context("not a snugly archive (too short)")?;
    let header = Header::decode(&hb)?;
    let cipher = match &header.kdf {
        Some(k) => {
            let pw = read_password(false)?;
            Some(Arc::new(Cipher::from_password(pw.as_bytes(), k)?))
        }
        None => None,
    };

    // Fast path: the footer at the very end.
    match index_at(&mut file, len, cipher.as_deref()) {
        Ok(index) => return Ok(Opened { file, header, cipher, index, end: len }),
        Err(e) => {
            // A real footer that fails to authenticate means a wrong password — don't go hunting.
            let tail = if len >= FOOTER_LEN { read_at(&mut file, len - FOOTER_LEN, FOOTER_LEN as usize)? } else { vec![] };
            if tail.len() == FOOTER_LEN as usize && &tail[24..32] == END_MAGIC {
                return Err(e);
            }
        }
    }

    // Interrupted update: scan backwards for the last complete footer.
    const WIN: u64 = 1 << 20;
    let mut hi = len;
    while hi > HEADER_LEN {
        let lo = hi.saturating_sub(WIN).max(HEADER_LEN);
        let buf = read_at(&mut file, lo, (hi - lo) as usize)?;
        for i in (0..buf.len().saturating_sub(END_MAGIC.len() - 1)).rev() {
            if &buf[i..i + END_MAGIC.len()] == END_MAGIC {
                let footer_end = lo + i as u64 + END_MAGIC.len() as u64;
                if let Ok(index) = index_at(&mut file, footer_end, cipher.as_deref()) {
                    eprintln!(
                        "warning: the last update was interrupted; using the last complete snapshot \
                         ({} trailing bytes ignored and reclaimed on the next update)",
                        len - footer_end
                    );
                    return Ok(Opened { file, header, cipher, index, end: footer_end });
                }
            }
        }
        if lo == HEADER_LEN {
            break;
        }
        hi = lo + END_MAGIC.len() as u64 - 1; // overlap so a magic split across windows is found
    }
    bail!("no complete snapshot found — archive is damaged or was never finished")
}
