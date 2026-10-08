//! Compile-time guard for the public sign-in API. Every type and function a
//! caller outside the crate needs to drive a login (QR, password, Steam
//! Guard) must be reachable as `aether_steam::...`. This file is never
//! executed — `_never_called` and `_type_assertions` are never invoked, and
//! `_never_called` would try a real network connection if it were — it only
//! has to compile. A public type added to `auth.rs` without a matching
//! `pub use` in `lib.rs` fails the build here with E0433, instead of only
//! surfacing when some downstream crate or example tries to use it.
#![allow(dead_code)]

use aether_steam::{
    CodeKind, CredentialFile, GuardChallenge, GuardOffer, LoginConfig, LoginMethod, LoginPoll,
    LoginPrompter, PasswordLogin, QrLogin, SteamCredentials, begin_password_login,
    login_interactive, render_qr,
};
use std::io;
use std::time::Duration;

/// Every name the coordinator's fix-round finding called out, explicitly
/// named as a type argument: if any of these is missing from `lib.rs`'s
/// `pub use auth::{...}`, the `use` above fails to resolve and this whole
/// file fails to compile.
fn _type_assertions() {
    fn assert_type<T>() {}
    assert_type::<CodeKind>();
    assert_type::<GuardChallenge>();
    assert_type::<GuardOffer>();
    assert_type::<LoginConfig>();
    assert_type::<LoginPoll>();
    assert_type::<PasswordLogin>();
    assert_type::<QrLogin>();
    assert_type::<LoginMethod>();
    assert_type::<CredentialFile>();
    // `begin_password_login`, `login_interactive` and `render_qr` are
    // functions, not types; they are exercised below instead.
}

/// A minimal external [`LoginPrompter`] implementation. Proves the trait,
/// including its required `code_kind` method, is implementable from outside
/// the crate.
struct MutePrompter;

impl LoginPrompter for MutePrompter {
    async fn show_qr(&mut self, _qr: &str, _url: &str) -> io::Result<()> {
        Ok(())
    }
    async fn password(&mut self, _account: &str) -> io::Result<String> {
        Ok(String::new())
    }
    async fn show_guard(&mut self, _offer: &GuardOffer) -> io::Result<()> {
        Ok(())
    }
    async fn next_code(&mut self) -> io::Result<String> {
        Ok(String::new())
    }
    async fn notice(&mut self, _msg: &str) -> io::Result<()> {
        Ok(())
    }
    async fn code_kind(&mut self, _offer: &GuardOffer) -> io::Result<CodeKind> {
        Ok(CodeKind::Email)
    }
}

/// Drives the whole public sign-in surface as an external caller would,
/// including `submit_code`'s `CodeKind` parameter — the exact call that
/// broke with E0433 when `CodeKind` was left out of `lib.rs`'s `pub use`.
/// Never executed: only type-checked.
async fn _never_called() {
    let cfg = LoginConfig {
        device_name: "test".into(),
        rpc_timeout: Duration::from_secs(1),
    };
    let _new_cfg: LoginConfig = LoginConfig::new("test");

    let mut qr: QrLogin = QrLogin::begin(&cfg).await.unwrap();
    let _url: &str = qr.challenge_url();
    let _interval: Duration = qr.poll_interval();
    match qr.poll().await.unwrap() {
        LoginPoll::Pending | LoginPoll::NewChallenge => {}
        LoginPoll::Done(creds) => {
            let _ = creds.account_name;
        }
    }

    let login: PasswordLogin = begin_password_login("account", "password", None, &cfg)
        .await
        .unwrap();
    match login {
        PasswordLogin::Done(_creds) => {}
        PasswordLogin::NeedsGuard(mut gc) => {
            let gc: &mut GuardChallenge = &mut gc;
            let offer: &GuardOffer = gc.offer();
            let _ = (
                offer.email_code,
                offer.totp_code,
                offer.mobile_approval,
                offer.email_approval,
                offer.email_domain.as_deref(),
                offer.accepts_code(),
            );
            let _interval: Duration = gc.poll_interval();
            gc.submit_code("000000", CodeKind::Email).await.unwrap();
            gc.submit_code("000000", CodeKind::Device).await.unwrap();
            let _poll: LoginPoll = gc.poll().await.unwrap();
        }
    }

    let _qr_text: String = render_qr("https://s.team/q/1/1").unwrap();

    let mut prompter = MutePrompter;
    let file = CredentialFile::new("/dev/null");
    let _creds: SteamCredentials = login_interactive(
        LoginMethod::Password {
            account: "account".into(),
        },
        &mut prompter,
        &file,
        &cfg,
        Duration::from_secs(1),
    )
    .await
    .unwrap();
}
