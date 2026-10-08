//! Chunk decoding: AES-256 decrypt, decompress, then verify size, Steam's
//! zero-seeded Adler-32 and SHA-1(content) == chunk id.
use crate::error::SteamError;
use crate::ids::DepotKey;
use crate::manifest::ChunkRef;
use steamroom::depot::chunk::ChunkCompression;
use steamroom::util::checksum::SteamAdler32;

/// The largest chunk this crate will decode. Real Steam chunks are a few
/// hundred KiB to ~1 MiB; this is generous headroom against a manifest that
/// declares a hostile `len`. Enforced both where chunks enter the crate
/// ([`DepotManifest::new`](crate::manifest::DepotManifest::new)) and again
/// here, defensively, before any bytes are decoded.
pub(crate) const MAX_CHUNK_LEN: u32 = 32 << 20;

/// Decode one chunk exactly as downloaded from the CDN. CPU-heavy (up to
/// ~1 MiB of zstd/LZMA): call from a blocking context.
///
/// Every path is bounded by `c.len` — the chunk's declared (manifest-trusted,
/// [`MAX_CHUNK_LEN`]-capped) decompressed size — never by anything read from
/// the untrusted CDN bytes. steamroom's own `process_chunk` already does
/// this for the Valve-zstd and Valve-LZMA formats (`expected_size` is
/// threaded through to the decompressor, and `zstd::bulk::decompress` errors
/// rather than over-allocates), so those are passed straight through. Its
/// zip path is not bounded (`read_to_end` with no limit), and raw LZMA
/// (rare; DepotDownloader/SteamKit never produce it) sizes its output from
/// an attacker-controlled header with no `expected_size` check at all — both
/// are handled here instead of reaching steamroom: zip decodes through a
/// length-capped reader of our own, raw LZMA is rejected outright.
pub(crate) fn decode_chunk(
    raw: &[u8],
    key: &DepotKey,
    c: &ChunkRef,
) -> Result<Vec<u8>, SteamError> {
    let bad = |msg: String| SteamError::Integrity(format!("chunk {}: {msg}", c.id));
    if c.len > MAX_CHUNK_LEN {
        return Err(bad(format!(
            "declared size {} exceeds the {MAX_CHUNK_LEN}-byte cap",
            c.len
        )));
    }
    if raw.len() < 32 {
        return Err(bad("too short to be a chunk".into()));
    }
    let sk = steamroom::depot::DepotKey(key.0);
    // Peek at the first decrypted payload block to identify the compression
    // format before committing to a decompressor. The chunk format is
    // ECB(IV, 16 bytes) || CBC(payload), so the first plaintext block is
    // ECB_decrypt(first ciphertext block) XOR IV — no need to CBC-decrypt
    // the rest just to look at the magic bytes.
    let iv = steamroom::crypto::symmetric_decrypt_ecb_nopad(&raw[..16], &sk.0)
        .map_err(|e| bad(e.to_string()))?;
    let first_cipher_block = steamroom::crypto::symmetric_decrypt_ecb_nopad(&raw[16..32], &sk.0)
        .map_err(|e| bad(e.to_string()))?;
    let mut first_block = first_cipher_block;
    for (b, k) in first_block.iter_mut().zip(&iv) {
        *b ^= k;
    }
    let plain = match ChunkCompression::detect(&first_block) {
        ChunkCompression::Zip => decode_zip_chunk(raw, &sk, c)?,
        ChunkCompression::Lzma => {
            return Err(bad(
                "raw LZMA chunks are not supported (unbounded decompressed size)".into(),
            ));
        }
        _ => steamroom::depot::chunk::process_chunk(raw, &sk, c.len, c.adler)
            .map_err(|e| bad(e.to_string()))?,
    };
    let sha = sha1_smol::Sha1::from(&plain).digest().bytes();
    if sha != c.id.0 {
        return Err(bad("SHA-1 of content does not match its id".into()));
    }
    Ok(plain)
}

/// Decrypt and decompress a zip-wrapped chunk ourselves rather than through
/// steamroom's `process_chunk`, whose zip path reads the entry to the end
/// with no bound. Reads at most `c.len + 1` decompressed bytes: enough to
/// tell "too big" apart from "exactly right" without ever holding more than
/// one chunk's worth of hostile output in memory.
fn decode_zip_chunk(
    raw: &[u8],
    key: &steamroom::depot::DepotKey,
    c: &ChunkRef,
) -> Result<Vec<u8>, SteamError> {
    let bad = |msg: String| SteamError::Integrity(format!("chunk {}: {msg}", c.id));
    let iv = steamroom::crypto::symmetric_decrypt_ecb_nopad(&raw[..16], &key.0)
        .map_err(|e| bad(e.to_string()))?;
    let decrypted = steamroom::crypto::symmetric_decrypt_cbc(&raw[16..], &key.0, &iv)
        .map_err(|e| bad(e.to_string()))?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&decrypted))
        .map_err(|e| bad(format!("zip: {e}")))?;
    if archive.is_empty() {
        return Err(bad("zip: empty archive".into()));
    }
    let entry = archive.by_index(0).map_err(|e| bad(format!("zip: {e}")))?;
    let mut out = Vec::new();
    let mut limited = std::io::Read::take(entry, u64::from(c.len) + 1);
    std::io::Read::read_to_end(&mut limited, &mut out).map_err(|e| bad(format!("zip: {e}")))?;
    if out.len() as u64 != u64::from(c.len) {
        return Err(bad(format!(
            "size mismatch: expected {}, got at least {}",
            c.len,
            out.len()
        )));
    }
    let checksum = SteamAdler32::compute(&out).0;
    if checksum != c.adler {
        return Err(bad(format!(
            "checksum mismatch: expected {:#010x}, got {:#010x}",
            c.adler, checksum
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{KEY, chunk_ref_for, encrypt_chunk};

    #[test]
    fn decodes_a_valid_chunk() {
        let data: Vec<u8> = (0..5000u32).map(|i| (i * 7) as u8).collect();
        let c = chunk_ref_for(&data, 0);
        let raw = encrypt_chunk(&data, &KEY);
        assert_eq!(decode_chunk(&raw, &KEY, &c).unwrap(), data);
    }

    #[test]
    fn rejects_wrong_key_bad_adler_and_wrong_id() {
        let data = b"hello chunk".repeat(50);
        let c = chunk_ref_for(&data, 0);
        let raw = encrypt_chunk(&data, &KEY);
        assert!(decode_chunk(&raw, &DepotKey([9; 32]), &c).is_err());
        let mut bad_adler = c;
        bad_adler.adler ^= 1;
        assert!(matches!(
            decode_chunk(&raw, &KEY, &bad_adler),
            Err(SteamError::Integrity(_))
        ));
        let mut bad_id = c;
        bad_id.id.0[0] ^= 1;
        let err = decode_chunk(&raw, &KEY, &bad_id).unwrap_err().to_string();
        assert!(err.contains("SHA-1"), "{err}");
        let mut bad_len = c;
        bad_len.len += 1;
        assert!(decode_chunk(&raw, &KEY, &bad_len).is_err());
        assert!(decode_chunk(&raw[..20], &KEY, &c).is_err());
    }

    /// AES-256 ECB(IV) || CBC(plain) — the same envelope [`encrypt_chunk`]
    /// uses, but around an arbitrary payload instead of a VSZa frame, so
    /// tests can control the decrypted magic bytes directly.
    fn encrypt_raw(plain: &[u8], key: &DepotKey) -> Vec<u8> {
        let iv = [0x11u8; 16];
        let mut out = steamroom::crypto::symmetric_encrypt_ecb_nopad(&iv, &key.0).unwrap();
        out.extend(steamroom::crypto::symmetric_encrypt_cbc(plain, &key.0, &iv).unwrap());
        out
    }

    #[test]
    fn rejects_a_zip_bomb_chunk_without_decompressing_it_fully() {
        // Highly compressible: a real zip bomb's compressed form is tiny
        // relative to what it inflates to.
        let inflated = vec![0u8; 8 << 20];
        let mut zip_bytes = Vec::new();
        {
            let mut zw = zip::ZipWriter::new(std::io::Cursor::new(&mut zip_bytes));
            zw.start_file(
                "z",
                zip::write::SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
            std::io::Write::write_all(&mut zw, &inflated).unwrap();
            zw.finish().unwrap();
        }
        assert!(zip_bytes.len() < 8 << 20, "fixture should compress well");
        let raw = encrypt_raw(&zip_bytes, &KEY);
        // The manifest declares far less than the archive actually inflates to.
        let mut c = chunk_ref_for(b"irrelevant, only .len is used below", 0);
        c.len = 100;
        let err = decode_chunk(&raw, &KEY, &c).unwrap_err();
        assert!(matches!(err, SteamError::Integrity(_)));
        let msg = err.to_string();
        assert!(msg.contains("size mismatch"), "{msg}");
    }

    #[test]
    fn rejects_raw_lzma_chunks() {
        let mut payload = vec![0x5Du8, 0, 0, 0, 0];
        payload.extend_from_slice(&[0u8; 27]);
        let raw = encrypt_raw(&payload, &KEY);
        let c = chunk_ref_for(b"unused", 0);
        let err = decode_chunk(&raw, &KEY, &c).unwrap_err().to_string();
        assert!(err.contains("LZMA"), "{err}");
    }

    #[test]
    fn rejects_a_chunk_declared_over_the_size_cap() {
        let data = b"x".repeat(10);
        let mut c = chunk_ref_for(&data, 0);
        c.len = MAX_CHUNK_LEN + 1;
        let raw = encrypt_chunk(&data, &KEY);
        let err = decode_chunk(&raw, &KEY, &c).unwrap_err();
        assert!(matches!(err, SteamError::Integrity(_)));
    }
}
