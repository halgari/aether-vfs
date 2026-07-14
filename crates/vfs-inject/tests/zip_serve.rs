//! Cross-process proof: an injected shim serves a target's read of a virtual
//! path DIRECTLY from a window inside a real Stored zip — nothing extracted.
mod common;

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use vfs_inject::{run_target_with_shim, RunConfig};

/// Write a minimal single-entry Stored zip and return (path, entry_name, content).
fn write_stored_zip(dir: &Path) -> std::path::PathBuf {
    // Reuse the same layout as vfs-zip's fixture writer.
    let name = "Data/proof.dat";
    let content = b"THESE-BYTES-LIVE-ONLY-INSIDE-THE-ZIP";
    let path = dir.join("mod.zip");
    let mut buf = Vec::new();
    let crc = crc32(content);
    let n = name.len() as u16;
    buf.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(name.as_bytes());
    let cd_off = buf.len() as u32;
    buf.extend_from_slice(content);
    let cd_start = buf.len() as u32;
    buf.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc.to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&(content.len() as u32).to_le_bytes());
    buf.extend_from_slice(&n.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 4]);
    buf.extend_from_slice(&0u32.to_le_bytes()); // local header offset
    buf.extend_from_slice(name.as_bytes());
    let cd_size = buf.len() as u32 - cd_start;
    buf.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&[0u8; 2]);
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&1u16.to_le_bytes());
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_start.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    let _ = cd_off;
    std::fs::File::create(&path).unwrap().write_all(&buf).unwrap();
    path
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[test]
fn injected_shim_serves_a_file_from_inside_a_zip() {
    let pid = std::process::id();
    let base = std::env::temp_dir().join(format!("vfs-zipe2e-{pid}"));
    let root = base.join("gameroot");
    std::fs::create_dir_all(&root).unwrap();

    let zip = write_stored_zip(&base);
    let expected = b"THESE-BYTES-LIVE-ONLY-INSIDE-THE-ZIP".to_vec();

    // Build the snapshot straight from the zip via vfs-zip.
    let layer = vfs_zip::read_layer(&zip, vfs_core::LayerId(0)).expect("read_layer");
    let tree = vfs_core::build(vec![layer]).unwrap();
    let snapshot = vfs_shared::bridge::flatten(&tree);

    let config_bytes = vfs_shim::encode_config(root.to_str().unwrap(), &snapshot);
    let config_path = base.join("shim.cfg");
    std::fs::write(&config_path, &config_bytes).unwrap();

    let ready_path = base.join("ready.flag");
    let _ = std::fs::remove_file(&ready_path);
    let output_path = base.join("probe-out.bin");
    let _ = std::fs::remove_file(&output_path);

    // The probe opens the VIRTUAL path (root/Data/proof.dat) and copies it out.
    let virtual_path = root.join("Data").join("proof.dat");
    let probe = env!("CARGO_BIN_EXE_vfs-probe").to_string();
    let (dll, payload) = common::locate_shim_and_payload();

    let exit = run_target_with_shim(RunConfig {
        target_exe: probe,
        args: vec![
            virtual_path.to_str().unwrap().to_string(),
            output_path.to_str().unwrap().to_string(),
        ],
        dll_path: dll,
        config_path: config_path.to_str().unwrap().to_string(),
        ready_path: ready_path.to_str().unwrap().to_string(),
        ready_timeout: Duration::from_secs(10),
        payload_path: payload,
        preinit_redirects: vec![],
    })
    .expect("run_target_with_shim");

    assert_eq!(exit, 0, "probe exit code");
    let got = std::fs::read(&output_path).expect("probe output");
    assert_eq!(got, expected, "served bytes must equal the zip entry content");

    // Prove nothing was extracted: the virtual path itself must not exist on
    // disk, and no file named `proof.dat` was materialized anywhere under
    // `base` (recursively — catches an extracted copy at gameroot/Data/proof.dat
    // or anywhere else a materialize-to-disk regression might write it).
    assert!(
        !virtual_path.exists(),
        "virtual path must not exist on disk — bytes must come from the zip window, not a materialized copy"
    );
    assert!(
        !contains_file_named(&base, "proof.dat"),
        "no entry should have been extracted to disk anywhere under the test root"
    );
}

fn contains_file_named(dir: &std::path::Path, needle: &str) -> bool {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if contains_file_named(&path, needle) {
                return true;
            }
        } else if entry.file_name().to_string_lossy() == needle {
            return true;
        }
    }
    false
}
