//! Extract selected entries from a spooled 7z or zip archive. Synchronous:
//! the caller runs it on a blocking thread over a store-backed `RangeRead`.
//!
//! The entry callback may fail with the caller's own error type `E`; this
//! module's own failures reach the caller as `E::from(FormatError)`.

use std::collections::HashMap;
use std::io::{self, Read};

use sevenz_rust2::{Archive, BlockDecoder, Password};

use crate::error::{FormatError, Result, invalid, unsupported};
use crate::path::fold;
use crate::range::{RangeCursor, RangeRead, read_vec};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    SevenZip,
    Zip,
    Rar,
}

/// Identify an archive by its magic bytes.
pub fn sniff<R: RangeRead + ?Sized>(r: &R) -> Result<ArchiveKind> {
    let head = read_vec(r, 0, r.len().min(8))?;
    if head.starts_with(b"7z\xBC\xAF\x27\x1C") {
        Ok(ArchiveKind::SevenZip)
    } else if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        Ok(ArchiveKind::Zip)
    } else if head.starts_with(b"Rar!\x1A\x07") {
        Ok(ArchiveKind::Rar)
    } else {
        Err(unsupported(
            "archive",
            format!("unknown archive format (starts with {head:02x?})"),
        ))
    }
}

/// Call `each(path, reader)` for every entry of the 7z or zip archive `r`
/// whose path matches one of `wanted` (Wabbajack paths: `\` or `/`, any
/// case). `path` is spelled as in `wanted`; `each` may read as much as it
/// likes (the rest is drained so checksums are still verified). RAR is
/// rejected as unsupported. Fails with [`FormatError::MissingEntries`] if
/// any wanted path is not in the archive. A 7z is decoded on one thread
/// ([`extract_threads`] for more).
pub fn extract<R, F, E>(r: R, wanted: &[String], each: F) -> std::result::Result<(), E>
where
    R: RangeRead,
    F: FnMut(&str, &mut dyn Read) -> std::result::Result<(), E>,
    E: From<FormatError>,
{
    extract_threads(r, wanted, 1, each)
}

/// [`extract`], decoding a 7z's LZMA2 blocks on up to `threads` threads.
///
/// Memory: one thread streams, holding one dictionary. More threads decode
/// the stream's independent chunks (LZMA2 dictionary resets) side by side,
/// and each holds its chunk decompressed whole, plus its own dictionary:
/// for an archive made by a multi-threaded 7-Zip that is hundreds of MiB per
/// thread (32 threads over 7-Zip's defaults: gigabytes). Keep `threads`
/// small.
pub fn extract_threads<R, F, E>(
    r: R,
    wanted: &[String],
    threads: u32,
    mut each: F,
) -> std::result::Result<(), E>
where
    R: RangeRead,
    F: FnMut(&str, &mut dyn Read) -> std::result::Result<(), E>,
    E: From<FormatError>,
{
    let mut want: HashMap<String, &str> = wanted.iter().map(|w| (fold(w), w.as_str())).collect();
    match sniff(&r)? {
        ArchiveKind::SevenZip => extract_7z(r, &mut want, threads.max(1), &mut each)?,
        ArchiveKind::Zip => extract_zip(r, &mut want, &mut each)?,
        ArchiveKind::Rar => {
            return Err(unsupported(
                "archive",
                "RAR archives from HTTP or CDN sources are not supported yet",
            )
            .into());
        }
    }
    if want.is_empty() {
        Ok(())
    } else {
        let mut missing: Vec<String> = want.into_values().map(str::to_string).collect();
        missing.sort();
        Err(FormatError::MissingEntries(missing).into())
    }
}

fn sevenz_err(e: sevenz_rust2::Error) -> FormatError {
    invalid("7z", e.to_string())
}

fn extract_7z<R: RangeRead, E: From<FormatError>>(
    r: R,
    want: &mut HashMap<String, &str>,
    threads: u32,
    each: &mut dyn FnMut(&str, &mut dyn Read) -> std::result::Result<(), E>,
) -> std::result::Result<(), E> {
    let mut src = RangeCursor::new(r);
    let password = Password::empty();
    let archive = Archive::read(&mut src, &password).map_err(sevenz_err)?;
    let mut failed: Option<E> = None;
    for block in 0..archive.blocks.len() {
        let decoder = BlockDecoder::new(threads, block, &archive, &password, &mut src);
        let mut left = decoder
            .entries()
            .iter()
            .filter(|f| !f.is_directory && want.contains_key(&fold(&f.name)))
            .count();
        if left == 0 {
            continue; // nothing wanted in this block: never decode it
        }
        decoder
            .for_each_entries(&mut |entry, reader| {
                if let Some(path) = (!entry.is_directory)
                    .then(|| want.remove(&fold(&entry.name)))
                    .flatten()
                {
                    left -= 1;
                    if let Err(e) = each(path, reader).and_then(|()| drain(reader).map_err(E::from))
                    {
                        failed = Some(e);
                        return Ok(false);
                    }
                } else {
                    // Solid blocks are one stream: skip by decoding.
                    io::copy(reader, &mut io::sink())?;
                }
                Ok(left > 0)
            })
            .map_err(sevenz_err)?;
        if let Some(e) = failed.take() {
            return Err(e);
        }
    }
    // Empty files have no block.
    for (i, f) in archive.files.iter().enumerate() {
        if archive.stream_map.file_block_index[i].is_none()
            && !f.is_directory
            && let Some(path) = want.remove(&fold(&f.name))
        {
            each(path, &mut io::empty())?;
        }
    }
    Ok(())
}

fn extract_zip<R: RangeRead, E: From<FormatError>>(
    r: R,
    want: &mut HashMap<String, &str>,
    each: &mut dyn FnMut(&str, &mut dyn Read) -> std::result::Result<(), E>,
) -> std::result::Result<(), E> {
    let zip_err = |e: zip::result::ZipError| invalid("zip", e.to_string());
    let mut z = zip::ZipArchive::new(RangeCursor::new(r)).map_err(zip_err)?;
    for i in 0..z.len() {
        if want.is_empty() {
            break;
        }
        let mut f = z.by_index(i).map_err(zip_err)?;
        if f.is_dir() {
            continue;
        }
        if let Some(path) = want.remove(&fold(f.name())) {
            each(path, &mut f)?;
            drain(&mut f)?; // the zip crate checks the CRC at end of entry
        }
    }
    Ok(())
}

fn drain(r: &mut dyn Read) -> Result<()> {
    io::copy(r, &mut io::sink())?;
    Ok(())
}
