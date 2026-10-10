//! Mount a GOG game's latest Windows build as a `DepotProvider` tree and
//! check it: walk it, read headers, and check whole-file MD5s against the
//! manifest (a small-files-container file and multi-chunk files read in
//! odd-sized pieces that straddle chunk boundaries).
//!
//! `cargo run -p aether-gog --features provider --example gog_mount -- <credentials file> [product]`

use aether_gog::provider::DepotProvider;
use aether_gog::{DepotItem, GogConfig, GogContent, Os, ProductId};
use aether_net::{Events, Http, HttpConfig};
use md5::Digest;
use vfs_provider::{KIND_DIR, OPEN_READ, Provider, VPath};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let creds = args
        .next()
        .ok_or("usage: gog_mount <credentials file> [product]")?;
    let product = ProductId(args.next().map_or(Ok(1711230643), |s| s.parse())?);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let cache = std::env::temp_dir().join("aether-gog-mount");
    let (content, depots) = rt.block_on(async {
        let http = Http::new(HttpConfig::default(), Events::default())?;
        let content = GogContent::open(http, GogConfig::new(&cache, &creds)).await?;
        let builds = content.builds(product, Os::Windows).await?;
        let build = &builds[0];
        println!("build {} ({})", build.build_id, build.version_name);
        let details = content.build_details(build).await?;
        let mut depots = Vec::new();
        for d in &details.depots {
            let keep = d
                .languages
                .iter()
                .any(|l| matches!(l.as_str(), "*" | "en" | "en-US" | "English"));
            println!(
                "  depot {} product {} {:?} {} bytes{}",
                d.manifest,
                d.product_id,
                d.languages,
                d.size,
                if keep { "" } else { " (skipped)" }
            );
            if keep {
                depots.push((d.product_id, content.depot(d).await?));
            }
        }
        Ok::<_, Box<dyn std::error::Error>>((content, depots))
    })?;
    let items: Vec<DepotItem> = depots.iter().flat_map(|(_, m)| m.items.clone()).collect();
    let provider = DepotProvider::new(content, depots, rt.handle().clone());

    // Provider calls block on the runtime, so make them from a plain thread.
    std::thread::scope(|s| s.spawn(|| check(&provider, &items)).join().unwrap())
        .map_err(|e| e as Box<dyn std::error::Error>)
}

fn check(
    p: &DepotProvider,
    items: &[DepotItem],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Walk the whole tree.
    let (mut files, mut bytes, mut stack) = (0u64, 0u64, vec![String::new()]);
    while let Some(dir) = stack.pop() {
        for e in p.readdir(VPath::at_default(&dir)).map_err(st)? {
            let path = if dir.is_empty() {
                e.name.clone()
            } else {
                format!("{dir}/{}", e.name)
            };
            if e.stat.kind == KIND_DIR {
                stack.push(path);
            } else {
                files += 1;
                bytes += e.stat.size;
            }
        }
    }
    println!(
        "tree: {files} files, {:.2} GiB",
        bytes as f64 / (1u64 << 30) as f64
    );
    let names: Vec<_> = p
        .readdir(VPath::at_default(""))
        .map_err(st)?
        .into_iter()
        .map(|e| e.name)
        .collect();
    println!("root: {names:?}");

    // Headers, looked up with the wrong case and separators.
    let head =
        |path: &str, n: usize| -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
            let (h, _, _) = p.open(VPath::at_default(path), OPEN_READ).map_err(st)?;
            let mut buf = vec![0u8; n];
            let got = p.read_at(h, 0, &mut buf).map_err(st)?;
            p.close(h).map_err(st)?;
            buf.truncate(got);
            Ok(buf)
        };
    let exe = head("skyrimse.EXE", 2)?;
    println!("SkyrimSE.exe starts {:?}", String::from_utf8_lossy(&exe));
    assert_eq!(exe, b"MZ");
    let bsa = head("DATA\\skyrim - misc.bsa", 4)?;
    println!("Skyrim - Misc.bsa starts {bsa:?}");
    assert_eq!(bsa, b"BSA\0");

    // Whole files against the manifest's MD5.
    let with_md5 = |i: &&DepotItem| i.md5.is_some() && i.size > 0;
    let mut picks: Vec<&DepotItem> = Vec::new();
    picks.extend(items.iter().filter(with_md5).find(|i| i.sfc_ref.is_some()));
    let mut multi: Vec<_> = items
        .iter()
        .filter(with_md5)
        .filter(|i| i.sfc_ref.is_none() && i.chunks.len() >= 2)
        .collect();
    multi.sort_by_key(|i| i.size);
    picks.extend(multi.iter().take(2));
    picks.extend(
        items
            .iter()
            .filter(with_md5)
            .find(|i| i.path.ends_with("SkyrimSE.exe")),
    );
    for item in picks {
        let t = std::time::Instant::now();
        let (h, size, _) = p
            .open(VPath::at_default(&item.path), OPEN_READ)
            .map_err(st)?;
        assert_eq!(size, item.size);
        let (mut md5, mut off, mut buf) = (md5::Md5::new(), 0u64, vec![0u8; (1 << 20) + 7]);
        loop {
            let n = p.read_at(h, off, &mut buf).map_err(st)?;
            if n == 0 {
                break;
            }
            md5.update(&buf[..n]);
            off += n as u64;
        }
        p.close(h).map_err(st)?;
        assert_eq!(off, item.size, "{}: short file", item.path);
        let got: [u8; 16] = md5.finalize().into();
        assert_eq!(Some(got), item.md5, "{}: MD5 mismatch", item.path);
        println!(
            "md5 ok: {} ({} bytes, {} chunks{}) in {:.1}s",
            item.path,
            item.size,
            item.chunks.len(),
            if item.sfc_ref.is_some() {
                ", small-files container"
            } else {
                ""
            },
            t.elapsed().as_secs_f64()
        );
    }
    println!("all checks passed");
    Ok(())
}

fn st(code: i32) -> Box<dyn std::error::Error + Send + Sync> {
    format!("provider status {code}").into()
}
