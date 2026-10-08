//! The test server and repack builder behave like the real hosts.
#[path = "../../aether-net/tests/common/mod.rs"]
mod common;

use aether_archive::zip::ZipIndex;
use common::repack::{RepackFile, data, repacked_zip};
use common::{Canned, start};

async fn get(url: &str, range: Option<&str>) -> (u16, Option<String>, Vec<u8>) {
    let mut req = reqwest::Client::new().get(url);
    if let Some(r) = range {
        req = req.header("range", r);
    }
    let resp = req.send().await.unwrap();
    let cr = resp
        .headers()
        .get("content-range")
        .map(|v| v.to_str().unwrap().to_string());
    (
        resp.status().as_u16(),
        cr,
        resp.bytes().await.unwrap().to_vec(),
    )
}

#[tokio::test]
async fn serves_ranges_like_the_real_hosts() {
    let s = start().await;
    s.put("/f", b"0123456789".to_vec());
    assert_eq!(
        get(&s.url("/f"), Some("bytes=2-4")).await,
        (206, Some("bytes 2-4/10".into()), b"234".to_vec())
    );
    assert_eq!(get(&s.url("/f"), None).await.2, b"0123456789");
    assert_eq!(get(&s.url("/f"), Some("bytes=10-12")).await.0, 416);
    s.no_range("/f");
    assert_eq!(get(&s.url("/f"), Some("bytes=2-4")).await.0, 200);
    s.redirect("/r", &s.url("/f"));
    assert_eq!(get(&s.url("/r"), None).await.2, b"0123456789");
    s.script(
        "/f",
        Canned {
            status: 503,
            headers: vec![],
            body: vec![],
        },
    );
    assert_eq!(get(&s.url("/f"), None).await.0, 503);
    assert_eq!(get(&s.url("/f"), None).await.0, 200);
    assert_eq!(s.requests("/f").len(), 7); // the redirect lands on /f too

    // Nexus's file host: signature checked, 500 (not 416) past EOF.
    s.put_repacked(7, b"abc".to_vec());
    assert_eq!(
        get(&s.url("/repacked/7?sig=1"), Some("bytes=0-0")).await.0,
        206
    );
    assert_eq!(get(&s.url("/repacked/7?sig=2"), None).await.0, 403);
    let (status, _, body) = get(&s.url("/repacked/7?sig=1"), Some("bytes=3-3")).await;
    assert_eq!((status, body), (500, b"error code: 1101".to_vec()));
}

#[test]
fn repacked_zips_have_the_nexus_shape() {
    let zip = repacked_zip(&[
        RepackFile::new("a.esp", data(1000, 1)),
        RepackFile::dir("d/"),
        RepackFile::new("d/big.dds", data(200_000, 2)).frames(64 << 10),
    ]);
    let idx = ZipIndex::read(&zip[..]).unwrap();
    let e = idx.entries();
    assert_eq!(e.len(), 3);
    assert_eq!((e[0].method, e[0].flags), (93, 0x0808));
    assert!(e[0].seek_table.is_none());
    assert!(e[1].is_dir() && e[1].method == 0);
    assert_eq!(e[2].seek_table.as_ref().unwrap().frames().len(), 4);
    // Local headers carry 56 bytes of extra fields; the central copy adds 0x4E58.
    let lh = e[0].local_header_offset as usize;
    assert_eq!(u16::from_le_bytes([zip[lh + 28], zip[lh + 29]]), 56);
}
