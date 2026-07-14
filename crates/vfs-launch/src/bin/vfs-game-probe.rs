//! Read a few virtual game paths and print sizes/head bytes (injected target).
use std::path::Path;

fn main() {
    let root = std::env::args().nth(1).expect("root");
    let out = std::env::args().nth(2).expect("out");
    let root = Path::new(&root);
    let mut report = String::new();

    let checks = [
        "Data/SkyUI_SE.esp",
        "Data/Skyrim.esm",
        "Data/Skyrim - Interface.bsa",
        "Skyrim_Default.ini",
        "Data",
    ];
    for rel in checks {
        let p = root.join(rel.replace('/', "\\"));
        match std::fs::metadata(&p) {
            Ok(m) => {
                report.push_str(&format!("meta {rel}: ok len={} dir={}\n", m.len(), m.is_dir()));
                if m.is_file() && m.len() > 0 {
                    match std::fs::File::open(&p) {
                        Ok(mut f) => {
                            use std::io::Read;
                            let mut buf = [0u8; 16];
                            match f.read(&mut buf) {
                                Ok(n) => report.push_str(&format!(
                                    "read {rel}: {n} bytes head={:02x?}\n",
                                    &buf[..n]
                                )),
                                Err(e) => report.push_str(&format!("read {rel}: ERR {e}\n")),
                            }
                        }
                        Err(e) => report.push_str(&format!("open {rel}: ERR {e}\n")),
                    }
                }
            }
            Err(e) => report.push_str(&format!("meta {rel}: ERR {e}\n")),
        }
    }

    // Directory listing of Data/
    match std::fs::read_dir(root.join("Data")) {
        Ok(rd) => {
            let mut names: Vec<_> = rd
                .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect();
            names.sort();
            report.push_str(&format!("readdir Data: {} entries\n", names.len()));
            for n in names.iter().take(15) {
                report.push_str(&format!("  {n}\n"));
            }
            if names.iter().any(|n| n == "SkyUI_SE.esp") {
                report.push_str("has SkyUI_SE.esp: yes\n");
            } else {
                report.push_str("has SkyUI_SE.esp: NO\n");
            }
        }
        Err(e) => report.push_str(&format!("readdir Data: ERR {e}\n")),
    }

    // Memory-map a modest BSA window (CreateFileMapping path).
    let bsa = root.join("Data").join("SkyUI_SE.bsa");
    match std::fs::File::open(&bsa) {
        Ok(f) => {
            use std::os::windows::io::AsRawHandle;
            // std::fs::File works; try memmap via windows API in a simple way:
            // just read full small bsa
            match std::fs::read(&bsa) {
                Ok(bytes) => report.push_str(&format!(
                    "fullread SkyUI_SE.bsa: {} bytes head={:02x?}\n",
                    bytes.len(),
                    &bytes[..bytes.len().min(8)]
                )),
                Err(e) => report.push_str(&format!("fullread SkyUI_SE.bsa: ERR {e}\n")),
            }
            let _ = f.as_raw_handle();
        }
        Err(e) => report.push_str(&format!("open SkyUI_SE.bsa: ERR {e}\n")),
    }

    std::fs::write(&out, &report).expect("write report");
    // Exit 0 only if key files worked.
    let ok = report.contains("meta Data/SkyUI_SE.esp: ok")
        && report.contains("meta Data/Skyrim.esm: ok")
        && report.contains("has SkyUI_SE.esp: yes");
    std::process::exit(if ok { 0 } else { 1 });
}
