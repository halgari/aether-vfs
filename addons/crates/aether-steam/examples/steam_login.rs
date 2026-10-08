//! `haskill login steam` until the real CLI exists:
//!
//! ```text
//! cargo run -p aether-steam --example steam_login              # QR code
//! cargo run -p aether-steam --example steam_login -- --password ACCOUNT
//! cargo run -p aether-steam --example steam_login -- --check    # silent re-login
//! ```
//!
//! Saves to `CredentialFile::default_path()`, or `$HASKILL_STEAM_LOGIN`.
use aether_steam::{
    CodeKind, CredentialFile, GuardOffer, LoginConfig, LoginMethod, LoginPrompter, SessionConfig,
    SteamError, SteamSession, login_interactive,
};
use std::io::{self, BufRead, Write};
use std::time::Duration;
use tokio::sync::mpsc;

/// Terminal prompter. Lines come from a dedicated stdin thread over a
/// channel, so waiting for input never blocks the runtime and `next_code`
/// is cancel safe.
struct Terminal {
    /// Started lazily, on first actual need for typed input — always after
    /// [`LoginPrompter::password`] has already run. `password` reads
    /// straight from the terminal (via `rpassword`) without going through
    /// this struct at all; if this background thread's blocking
    /// `stdin().lock().lines()` loop were already running at that point, it
    /// would be racing `rpassword` to read the very next line from the same
    /// terminal, and could end up swallowing the typed password and handing
    /// it back later as a Steam Guard code. Starting the reader only when
    /// something other than the password is actually being read rules that
    /// out structurally, not just by ordering the calls carefully.
    lines: Option<mpsc::UnboundedReceiver<String>>,
    /// Whether Steam is still waiting on a typed code, so a status line can
    /// re-show the "Code: " prompt after it.
    awaiting_code: bool,
}

impl Terminal {
    fn new() -> Self {
        Terminal {
            lines: None,
            awaiting_code: false,
        }
    }

    fn lines(&mut self) -> &mut mpsc::UnboundedReceiver<String> {
        self.lines.get_or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            std::thread::spawn(move || {
                for line in io::stdin().lock().lines() {
                    let Ok(line) = line else { return };
                    if tx.send(line).is_err() {
                        return;
                    }
                }
            });
            rx
        })
    }
}

impl LoginPrompter for Terminal {
    async fn show_qr(&mut self, qr: &str, _url: &str) -> io::Result<()> {
        // Never print the challenge URL itself: anyone who sees or copies it
        // can sign in as this account the same way a scanned QR code would
        // let them. Only the rendered code (meant to be read by a camera,
        // not retyped) is shown.
        eprintln!("\nScan this with the Steam mobile app (Steam Guard > scan a QR code):\n\n{qr}");
        Ok(())
    }

    async fn password(&mut self, account: &str) -> io::Result<String> {
        let prompt = format!("Steam password for {account}: ");
        // rpassword disables terminal echo for the duration of this read
        // and restores it afterward; a Ctrl-C here is handled by the
        // process exiting (rpassword does not install its own SIGINT
        // handler), which can leave the terminal without echo. If a prompt
        // afterward looks invisible, running `stty sane` restores it.
        tokio::task::spawn_blocking(move || rpassword::prompt_password(prompt))
            .await
            .map_err(io::Error::other)?
    }

    async fn show_guard(&mut self, offer: &GuardOffer) -> io::Result<()> {
        let mut ways = Vec::new();
        if offer.totp_code {
            ways.push("type the code from your Steam mobile authenticator".to_string());
        }
        if offer.email_code {
            let to = offer
                .email_domain
                .as_deref()
                .map(|d| format!(" (@{d})"))
                .unwrap_or_default();
            ways.push(format!("type the code Steam e-mailed you{to}"));
        }
        if offer.mobile_approval {
            ways.push("approve the sign-in in the Steam mobile app".into());
        }
        if offer.email_approval {
            ways.push("click the link Steam e-mailed you".into());
        }
        eprintln!("Steam Guard: {}.", ways.join(", or "));
        self.awaiting_code = offer.accepts_code();
        if self.awaiting_code {
            eprint!("Code: ");
            io::stderr().flush()?;
        }
        Ok(())
    }

    async fn next_code(&mut self) -> io::Result<String> {
        match self.lines().recv().await {
            Some(l) => Ok(l),
            None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "stdin closed")),
        }
    }

    async fn notice(&mut self, msg: &str) -> io::Result<()> {
        eprintln!("{msg}");
        // A rejected code is the common case this fires for; re-show the
        // prompt so it is clear another code is expected.
        if self.awaiting_code {
            eprint!("Code: ");
            io::stderr().flush()?;
        }
        Ok(())
    }

    /// Called only when Steam offers both an e-mail and an authenticator
    /// code and the user has just typed one of them.
    async fn code_kind(&mut self, _offer: &GuardOffer) -> io::Result<CodeKind> {
        loop {
            eprint!("Is that code from your (e)mail or your authenticator (d)evice? ");
            io::stderr().flush()?;
            let line = self.next_code().await?;
            match line.trim().to_ascii_lowercase().as_str() {
                "e" | "email" => return Ok(CodeKind::Email),
                "d" | "device" => return Ok(CodeKind::Device),
                _ => eprintln!("Please type 'e' or 'd'."),
            }
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), SteamError> {
    let file = match std::env::var_os("HASKILL_STEAM_LOGIN") {
        Some(p) => CredentialFile::new(p),
        None => CredentialFile::new(CredentialFile::default_path()?),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let method = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => LoginMethod::Qr,
        ["--password", account] => LoginMethod::Password {
            account: account.to_string(),
        },
        ["--check"] => {
            let creds = file.load()?.ok_or(SteamError::NotLoggedIn)?;
            let s = SteamSession::login(&creds, SessionConfig::default()).await?;
            eprintln!("Logged in to Steam as {}.", s.account().unwrap_or("?"));
            return Ok(());
        }
        _ => {
            eprintln!("usage: steam_login [--password ACCOUNT | --check]");
            std::process::exit(2);
        }
    };
    let mut term = Terminal::new();
    let creds = login_interactive(
        method,
        &mut term,
        &file,
        &LoginConfig::default(),
        Duration::from_secs(600),
    )
    .await?;
    // Prove the token works the way later runs will use it.
    SteamSession::login(&creds, SessionConfig::default()).await?;
    eprintln!(
        "Logged in to Steam as {}. Saved to {}.",
        creds.account_name,
        file.path().display()
    );
    Ok(())
}
