//! Log in to GOG and save the tokens for the live tests:
//! `cargo run -p aether-gog --example gog_login -- <credentials file>`.
use std::io::{BufRead, Write};

use aether_gog::{GogConfig, complete_login, login_url};
use aether_net::{Events, Http, HttpConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: gog_login <credentials file>")?;
    let cfg = GogConfig::new(std::env::temp_dir().join("aether-gog-login"), &path);
    println!("Open this address, log in, then paste the address of the page GOG shows:\n");
    println!("{}\n", login_url(&cfg));
    print!("> ");
    std::io::stdout().flush()?;
    let mut pasted = String::new();
    std::io::stdin().lock().read_line(&mut pasted)?;
    let http = Http::new(HttpConfig::default(), Events::default())?;
    complete_login(&http, &cfg, pasted.trim()).await?;
    println!("saved {path}");
    Ok(())
}
