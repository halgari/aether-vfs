//! The interactive Steam login flow as a library function. The host's CLI
//! supplies a [`LoginPrompter`] (terminal I/O); this module drives the
//! sign-in steps in order, never overlapping two requests, and saves the
//! result to the credential file the host names.
use crate::auth::{
    CodeKind, GuardChallenge, GuardOffer, LoginConfig, LoginPoll, PasswordLogin, QrLogin,
    begin_password_login,
};
use crate::credentials::{CredentialFile, SteamCredentials};
use crate::error::SteamError;
use std::future::Future;
use std::io;
use std::time::Duration;

/// How to sign in.
#[derive(Clone, Debug)]
pub enum LoginMethod {
    /// Scan a QR code with the Steam mobile app.
    Qr,
    /// Account name and password, then Steam Guard if the account uses it.
    Password { account: String },
}

/// The user-facing side of a sign-in. Implementations must not block the
/// async runtime: read the terminal on a separate thread and hand lines over
/// a channel.
pub trait LoginPrompter: Send {
    /// Show `qr` (rendered text) for `url`. Called again when Steam rotates
    /// the code. `url` is the live sign-in link for this account: it must
    /// never be printed or logged, only rendered into `qr` for scanning.
    fn show_qr(&mut self, qr: &str, url: &str) -> impl Future<Output = io::Result<()>> + Send;
    /// Ask for the password of `account`, without echo.
    fn password(&mut self, account: &str) -> impl Future<Output = io::Result<String>> + Send;
    /// Explain which Steam Guard options apply (once per challenge).
    fn show_guard(&mut self, offer: &GuardOffer) -> impl Future<Output = io::Result<()>> + Send;
    /// The next line the user types (a code, or empty). Must be cancel
    /// safe: the driver drops this future to poll Steam in between.
    fn next_code(&mut self) -> impl Future<Output = io::Result<String>> + Send;
    /// A one-line status message, e.g. "code rejected".
    fn notice(&mut self, msg: &str) -> impl Future<Output = io::Result<()>> + Send;

    /// Ask which kind of Steam Guard code the user is about to type. The
    /// driver ([`drive_guard`]) calls this only when it is genuinely
    /// ambiguous: Steam offered *both* an e-mail code and an authenticator
    /// code, and the user has just typed one. When Steam offers only one
    /// code kind, the driver uses it directly and never calls this method.
    /// There is no default: an implementation that cannot ask (and is never
    /// used with an account offering both kinds) may simply
    /// `unreachable!()`, but a general-purpose prompter must ask the user.
    fn code_kind(
        &mut self,
        offer: &GuardOffer,
    ) -> impl Future<Output = io::Result<CodeKind>> + Send;
}

/// Render `url` as a QR code of Unicode half blocks, two modules per
/// character row, with a quiet zone, dark modules drawn light-on-dark
/// terminal style (inverted) so phones read it on dark terminals.
pub fn render_qr(url: &str) -> Result<String, SteamError> {
    use qrcode::render::unicode::Dense1x2;
    let code = qrcode::QrCode::new(url.as_bytes())
        .map_err(|e| SteamError::Protocol(format!("cannot draw QR code: {e}")))?;
    Ok(code
        .render::<Dense1x2>()
        .dark_color(Dense1x2::Light)
        .light_color(Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

/// Run a whole sign-in: prompt, drive Steam, save the credentials to `file`
/// (mode 0600) and return them. Gives up after `deadline`.
pub async fn login_interactive<P: LoginPrompter>(
    method: LoginMethod,
    prompter: &mut P,
    file: &CredentialFile,
    cfg: &LoginConfig,
    deadline: Duration,
) -> Result<SteamCredentials, SteamError> {
    let run = async {
        match method {
            LoginMethod::Qr => loop {
                let mut qr = QrLogin::begin(cfg).await?;
                match drive_qr(&mut qr, prompter).await {
                    Err(SteamError::AuthSessionExpired) => {
                        prompter
                            .notice("The QR code expired; here is a new one.")
                            .await?;
                    }
                    other => return other,
                }
            },
            LoginMethod::Password { account } => {
                let password = prompter.password(&account).await?;
                // Reuse Steam Guard's "remember this device" data from an
                // earlier sign-in of the same account, if there is one.
                let saved = file.load().ok().flatten();
                let guard = saved
                    .as_ref()
                    .filter(|c| c.account_name.eq_ignore_ascii_case(&account))
                    .and_then(|c| c.guard_data.as_deref());
                match begin_password_login(&account, &password, guard, cfg).await? {
                    PasswordLogin::Done(c) => Ok(c),
                    PasswordLogin::NeedsGuard(mut g) => drive_guard(&mut g, prompter).await,
                }
            }
        }
    };
    let creds = tokio::time::timeout(deadline, run)
        .await
        .map_err(|_| SteamError::Timeout {
            what: "the Steam sign-in to be approved",
            after: deadline,
        })??;
    file.save(&creds)?;
    Ok(creds)
}

/// The polling side of a QR sign-in, abstracted for tests.
pub(crate) trait QrFlow: Send {
    fn challenge_url(&self) -> &str;
    fn poll_interval(&self) -> Duration;
    fn poll(&mut self) -> impl Future<Output = Result<LoginPoll, SteamError>> + Send;
}

impl QrFlow for QrLogin {
    fn challenge_url(&self) -> &str {
        QrLogin::challenge_url(self)
    }
    fn poll_interval(&self) -> Duration {
        QrLogin::poll_interval(self)
    }
    async fn poll(&mut self) -> Result<LoginPoll, SteamError> {
        QrLogin::poll(self).await
    }
}

/// The Steam Guard side of a password sign-in, abstracted for tests. Its
/// `submit_code` takes the same [`CodeKind`] the real
/// [`GuardChallenge::submit_code`] does, so [`drive_guard`] threads through
/// exactly the kind it resolved (see [`LoginPrompter::code_kind`]) with no
/// extra adapter state in between.
pub(crate) trait GuardFlow: Send {
    fn offer(&self) -> &GuardOffer;
    fn poll_interval(&self) -> Duration;
    fn submit_code(
        &mut self,
        code: &str,
        kind: CodeKind,
    ) -> impl Future<Output = Result<(), SteamError>> + Send;
    fn poll(&mut self) -> impl Future<Output = Result<LoginPoll, SteamError>> + Send;
}

impl GuardFlow for GuardChallenge {
    fn offer(&self) -> &GuardOffer {
        GuardChallenge::offer(self)
    }
    fn poll_interval(&self) -> Duration {
        GuardChallenge::poll_interval(self)
    }
    async fn submit_code(&mut self, code: &str, kind: CodeKind) -> Result<(), SteamError> {
        GuardChallenge::submit_code(self, code, kind).await
    }
    async fn poll(&mut self) -> Result<LoginPoll, SteamError> {
        GuardChallenge::poll(self).await
    }
}

pub(crate) async fn drive_qr<F: QrFlow, P: LoginPrompter>(
    flow: &mut F,
    p: &mut P,
) -> Result<SteamCredentials, SteamError> {
    let url = flow.challenge_url().to_string();
    p.show_qr(&render_qr(&url)?, &url).await?;
    loop {
        tokio::time::sleep(flow.poll_interval()).await;
        match flow.poll().await? {
            LoginPoll::Pending => {}
            LoginPoll::NewChallenge => {
                let url = flow.challenge_url().to_string();
                p.show_qr(&render_qr(&url)?, &url).await?;
            }
            LoginPoll::Done(c) => return Ok(c),
        }
    }
}

/// The [`CodeKind`] to submit, when it can be determined without asking:
/// Steam offered exactly one code kind. `None` means both are offered (ask
/// the user) or neither is (no code will ever be submitted, so it does not
/// matter).
fn unambiguous_kind(offer: &GuardOffer) -> Option<CodeKind> {
    match (offer.email_code, offer.totp_code) {
        (true, false) => Some(CodeKind::Email),
        (false, true) => Some(CodeKind::Device),
        _ => None,
    }
}

pub(crate) async fn drive_guard<G: GuardFlow, P: LoginPrompter>(
    g: &mut G,
    p: &mut P,
) -> Result<SteamCredentials, SteamError> {
    p.show_guard(g.offer()).await?;
    // Once stdin hits EOF while Steam also offers an out-of-band approval
    // (mobile app or e-mail link), stop trying to read codes and fall back
    // to polling only, rather than aborting the whole sign-in.
    let mut takes_code = g.offer().accepts_code();
    loop {
        let typed = if takes_code {
            tokio::select! {
                line = p.next_code() => match line {
                    Ok(l) => Some(l),
                    Err(_) if g.offer().mobile_approval || g.offer().email_approval => {
                        takes_code = false;
                        p.notice("No more input; waiting for approval instead.").await?;
                        None
                    }
                    Err(e) => return Err(e.into()),
                },
                _ = tokio::time::sleep(g.poll_interval()) => None,
            }
        } else {
            tokio::time::sleep(g.poll_interval()).await;
            None
        };
        if let Some(code) = typed.filter(|c| !c.trim().is_empty()) {
            // Resolved fresh for every submission (not cached): if the last
            // attempt was rejected, the user gets asked again rather than
            // being stuck resubmitting the same wrong kind until the
            // deadline.
            let kind = match unambiguous_kind(g.offer()) {
                Some(k) => k,
                None => p.code_kind(g.offer()).await?,
            };
            match g.submit_code(&code, kind).await {
                Ok(()) => {}
                Err(SteamError::InvalidGuardCode) => {
                    p.notice("Steam did not accept that code; enter it again.")
                        .await?;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
        if let LoginPoll::Done(c) = g.poll().await? {
            return Ok(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct Script {
        qrs: Vec<String>,
        guards: usize,
        notices: Vec<String>,
        codes: VecDeque<String>,
        /// Answers `code_kind` hands out, in order.
        kind_answers: VecDeque<CodeKind>,
        /// How many times `code_kind` was actually called.
        kind_asks: usize,
        /// `next_code` reports stdin EOF once `codes` is drained, instead of
        /// hanging forever.
        eof_after_codes: bool,
    }

    impl LoginPrompter for Script {
        async fn show_qr(&mut self, qr: &str, url: &str) -> io::Result<()> {
            assert!(!qr.is_empty());
            self.qrs.push(url.to_string());
            Ok(())
        }
        async fn password(&mut self, _account: &str) -> io::Result<String> {
            Ok("hunter2".into())
        }
        async fn show_guard(&mut self, _offer: &GuardOffer) -> io::Result<()> {
            self.guards += 1;
            Ok(())
        }
        async fn next_code(&mut self) -> io::Result<String> {
            match self.codes.pop_front() {
                Some(c) => Ok(c),
                None if self.eof_after_codes => {
                    Err(io::Error::new(io::ErrorKind::UnexpectedEof, "stdin closed"))
                }
                None => std::future::pending().await,
            }
        }
        async fn notice(&mut self, msg: &str) -> io::Result<()> {
            self.notices.push(msg.to_string());
            Ok(())
        }
        async fn code_kind(&mut self, _offer: &GuardOffer) -> io::Result<CodeKind> {
            self.kind_asks += 1;
            Ok(self.kind_answers.pop_front().unwrap_or(CodeKind::Email))
        }
    }

    fn done(account: &str) -> LoginPoll {
        LoginPoll::Done(SteamCredentials::new(account.into(), "tok".into(), None))
    }

    struct FakeQr {
        url: String,
        polls: VecDeque<LoginPoll>,
    }

    impl QrFlow for FakeQr {
        fn challenge_url(&self) -> &str {
            &self.url
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_millis(1)
        }
        async fn poll(&mut self) -> Result<LoginPoll, SteamError> {
            let next = self.polls.pop_front().expect("polled after Done");
            if let LoginPoll::NewChallenge = next {
                self.url = "https://s.team/q/2".into();
            }
            Ok(next)
        }
    }

    #[tokio::test]
    async fn qr_rerenders_when_the_challenge_rotates() {
        let mut flow = FakeQr {
            url: "https://s.team/q/1".into(),
            polls: VecDeque::from([
                LoginPoll::Pending,
                LoginPoll::NewChallenge,
                LoginPoll::Pending,
                done("alice"),
            ]),
        };
        let mut p = Script::default();
        let c = drive_qr(&mut flow, &mut p).await.unwrap();
        assert_eq!(c.account_name, "alice");
        assert_eq!(p.qrs, ["https://s.team/q/1", "https://s.team/q/2"]);
    }

    struct FakeGuard {
        offer: GuardOffer,
        good_code: Option<&'static str>,
        good_kind: CodeKind,
        accepted: bool,
        polls_until_done: usize,
        submitted: Vec<String>,
        submitted_kinds: Vec<CodeKind>,
    }

    impl GuardFlow for FakeGuard {
        fn offer(&self) -> &GuardOffer {
            &self.offer
        }
        fn poll_interval(&self) -> Duration {
            Duration::from_millis(5)
        }
        async fn submit_code(&mut self, code: &str, kind: CodeKind) -> Result<(), SteamError> {
            self.submitted.push(code.to_string());
            self.submitted_kinds.push(kind);
            if Some(code) == self.good_code && kind == self.good_kind {
                self.accepted = true;
                Ok(())
            } else {
                Err(SteamError::InvalidGuardCode)
            }
        }
        async fn poll(&mut self) -> Result<LoginPoll, SteamError> {
            if self.good_code.is_some() && !self.accepted {
                return Ok(LoginPoll::Pending);
            }
            if self.polls_until_done == 0 {
                return Ok(done("bob"));
            }
            self.polls_until_done -= 1;
            Ok(LoginPoll::Pending)
        }
    }

    #[tokio::test]
    async fn wrong_code_is_reprompted_then_accepted() {
        let mut g = FakeGuard {
            offer: GuardOffer {
                email_code: true,
                ..GuardOffer::default()
            },
            good_code: Some("GOOD1"),
            good_kind: CodeKind::Email,
            accepted: false,
            polls_until_done: 0,
            submitted: vec![],
            submitted_kinds: vec![],
        };
        let mut p = Script {
            codes: VecDeque::from(["BAD00".to_string(), "".to_string(), "GOOD1".to_string()]),
            ..Script::default()
        };
        let c = drive_guard(&mut g, &mut p).await.unwrap();
        assert_eq!(c.account_name, "bob");
        assert_eq!(
            g.submitted,
            ["BAD00", "GOOD1"],
            "empty lines are not submitted"
        );
        assert_eq!(p.notices.len(), 1);
        assert_eq!(p.guards, 1);
        // Only one code kind was offered: never asked.
        assert_eq!(p.kind_asks, 0);
        assert_eq!(g.submitted_kinds, [CodeKind::Email, CodeKind::Email]);
    }

    #[tokio::test]
    async fn mobile_approval_needs_no_input() {
        let mut g = FakeGuard {
            offer: GuardOffer {
                mobile_approval: true,
                ..GuardOffer::default()
            },
            good_code: None,
            good_kind: CodeKind::Email,
            accepted: false,
            polls_until_done: 3,
            submitted: vec![],
            submitted_kinds: vec![],
        };
        let mut p = Script::default();
        drive_guard(&mut g, &mut p).await.unwrap();
        assert!(g.submitted.is_empty());
        assert_eq!(p.kind_asks, 0);
    }

    #[tokio::test]
    async fn unrecognized_guard_type_completes_by_polling_alone() {
        // Steam offered only a type this crate does not know how to act on
        // (e.g. type 6, MachineToken): `guard_offer` turns that into an
        // all-false `GuardOffer` rather than `NoSupportedGuard`, and the
        // driver must still reach completion by polling, asking for no
        // input at all.
        let mut g = FakeGuard {
            offer: GuardOffer::default(),
            good_code: None,
            good_kind: CodeKind::Email,
            accepted: false,
            polls_until_done: 3,
            submitted: vec![],
            submitted_kinds: vec![],
        };
        let mut p = Script::default();
        let c = drive_guard(&mut g, &mut p).await.unwrap();
        assert_eq!(c.account_name, "bob");
        assert!(g.submitted.is_empty());
        assert_eq!(p.kind_asks, 0);
    }

    #[tokio::test]
    async fn code_or_approval_whichever_comes_first() {
        // Code accepted by Steam, but the user never types: approval wins.
        let mut g = FakeGuard {
            offer: GuardOffer {
                totp_code: true,
                mobile_approval: true,
                ..GuardOffer::default()
            },
            good_code: None,
            good_kind: CodeKind::Device,
            accepted: false,
            polls_until_done: 2,
            submitted: vec![],
            submitted_kinds: vec![],
        };
        let mut p = Script::default(); // next_code never resolves
        drive_guard(&mut g, &mut p).await.unwrap();
    }

    #[test]
    fn qr_renders_as_half_blocks() {
        let qr = render_qr("https://s.team/q/1/1234567890123456789").unwrap();
        assert!(qr.contains('█') || qr.contains('▀') || qr.contains('▄'));
        let widths: Vec<usize> = qr.lines().map(|l| l.chars().count()).collect();
        assert!(widths.iter().all(|&w| w == widths[0]), "square");
        // Two modules per text row: rows ≈ width / 2.
        assert!(
            (widths[0] / 2).abs_diff(widths.len()) <= 1,
            "{} rows, {} cols",
            widths.len(),
            widths[0]
        );
    }

    #[tokio::test]
    async fn kind_is_asked_only_once_both_offered_and_after_a_code_is_typed() {
        let mut g = FakeGuard {
            offer: GuardOffer {
                email_code: true,
                totp_code: true,
                ..GuardOffer::default()
            },
            good_code: Some("GOOD1"),
            good_kind: CodeKind::Device,
            accepted: false,
            polls_until_done: 0,
            submitted: vec![],
            submitted_kinds: vec![],
        };
        let mut p = Script {
            // The empty line must not trigger a kind ask (no code submitted
            // for it); only the real code does.
            codes: VecDeque::from(["".to_string(), "GOOD1".to_string()]),
            kind_answers: VecDeque::from([CodeKind::Device]),
            ..Script::default()
        };
        let c = drive_guard(&mut g, &mut p).await.unwrap();
        assert_eq!(c.account_name, "bob");
        assert_eq!(g.submitted, ["GOOD1"]);
        assert_eq!(g.submitted_kinds, [CodeKind::Device]);
        assert_eq!(p.kind_asks, 1);
    }

    #[tokio::test]
    async fn approval_completes_without_asking_kind_when_both_code_kinds_are_offered() {
        let mut g = FakeGuard {
            offer: GuardOffer {
                totp_code: true,
                email_code: true,
                mobile_approval: true,
                ..GuardOffer::default()
            },
            good_code: None,
            good_kind: CodeKind::Email,
            accepted: false,
            polls_until_done: 2,
            submitted: vec![],
            submitted_kinds: vec![],
        };
        let mut p = Script::default(); // next_code never resolves
        drive_guard(&mut g, &mut p).await.unwrap();
        assert!(g.submitted.is_empty());
        assert_eq!(p.kind_asks, 0, "no code was ever typed, so never asked");
    }

    #[tokio::test]
    async fn wrong_kind_is_rejected_then_the_kind_is_re_asked() {
        let mut g = FakeGuard {
            offer: GuardOffer {
                email_code: true,
                totp_code: true,
                ..GuardOffer::default()
            },
            good_code: Some("GOOD1"),
            good_kind: CodeKind::Email,
            accepted: false,
            polls_until_done: 0,
            submitted: vec![],
            submitted_kinds: vec![],
        };
        let mut p = Script {
            codes: VecDeque::from(["GOOD1".to_string(), "GOOD1".to_string()]),
            // First guess is wrong (Device when Steam wants Email); asked
            // again after the rejection and gets it right.
            kind_answers: VecDeque::from([CodeKind::Device, CodeKind::Email]),
            ..Script::default()
        };
        let c = drive_guard(&mut g, &mut p).await.unwrap();
        assert_eq!(c.account_name, "bob");
        assert_eq!(g.submitted, ["GOOD1", "GOOD1"]);
        assert_eq!(g.submitted_kinds, [CodeKind::Device, CodeKind::Email]);
        assert_eq!(p.kind_asks, 2);
        assert_eq!(p.notices.len(), 1);
    }

    #[tokio::test]
    async fn the_resolved_kind_reaches_submit_code_unchanged() {
        // Single offered kind: no ask, but the derived kind must still be
        // the one actually passed to `GuardFlow::submit_code` (the real
        // `impl GuardFlow for GuardChallenge` is a direct, one-line forward
        // of this same argument to `GuardChallenge::submit_code`, so this
        // is the effective coverage for that path too).
        let mut g = FakeGuard {
            offer: GuardOffer {
                totp_code: true,
                ..GuardOffer::default()
            },
            good_code: Some("000000"),
            good_kind: CodeKind::Device,
            accepted: false,
            polls_until_done: 0,
            submitted: vec![],
            submitted_kinds: vec![],
        };
        let mut p = Script {
            codes: VecDeque::from(["000000".to_string()]),
            ..Script::default()
        };
        drive_guard(&mut g, &mut p).await.unwrap();
        assert_eq!(g.submitted_kinds, [CodeKind::Device]);
        assert_eq!(p.kind_asks, 0);
    }

    #[tokio::test]
    async fn stdin_eof_falls_back_to_polling_when_approval_is_also_offered() {
        let mut g = FakeGuard {
            offer: GuardOffer {
                email_code: true,
                mobile_approval: true,
                ..GuardOffer::default()
            },
            good_code: None,
            good_kind: CodeKind::Email,
            accepted: false,
            polls_until_done: 2,
            submitted: vec![],
            submitted_kinds: vec![],
        };
        let mut p = Script {
            eof_after_codes: true,
            ..Script::default()
        };
        let c = drive_guard(&mut g, &mut p).await.unwrap();
        assert_eq!(c.account_name, "bob");
        assert!(g.submitted.is_empty());
        assert!(
            p.notices.iter().any(|n| n.contains("approval")),
            "{:?}",
            p.notices
        );
    }

    #[tokio::test]
    async fn stdin_eof_aborts_when_no_approval_is_offered() {
        let mut g = FakeGuard {
            offer: GuardOffer {
                email_code: true,
                ..GuardOffer::default()
            },
            good_code: Some("GOOD1"),
            good_kind: CodeKind::Email,
            accepted: false,
            polls_until_done: 0,
            submitted: vec![],
            submitted_kinds: vec![],
        };
        let mut p = Script {
            eof_after_codes: true,
            ..Script::default()
        };
        let err = drive_guard(&mut g, &mut p).await.unwrap_err();
        assert!(matches!(err, SteamError::Io(_)), "{err:?}");
    }
}
