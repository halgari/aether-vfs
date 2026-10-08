//! Encrypted app tickets (`k_EMsgClientRequestEncryptedAppTicket`, 5526 →
//! 5527), minted by Steam for the logged-on account and an app. This is what
//! a game's `ISteamUser::RequestEncryptedAppTicket` relays, so the bytes are
//! the same as `GetEncryptedAppTicket` returns: the serialized
//! `EncryptedAppTicket` message. steamroom ships the `.proto` but does not
//! generate these types, so they are declared here.
use crate::error::SteamError;
use prost::Message;
use zeroize::Zeroizing;

/// `k_EMsgClientRequestEncryptedAppTicket`.
pub(crate) const EMSG_REQUEST: u32 = 5526;
/// `k_EMsgClientRequestEncryptedAppTicketResponse`.
pub(crate) const EMSG_RESPONSE: u32 = 5527;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RequestEncryptedAppTicket {
    #[prost(uint32, optional, tag = "1")]
    pub app_id: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub userdata: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct EncryptedAppTicket {
    #[prost(uint32, optional, tag = "1")]
    pub ticket_version_no: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub crc_encryptedticket: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub cb_encrypteduserdata: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub cb_encrypted_appownershipticket: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "5")]
    pub encrypted_ticket: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct RequestEncryptedAppTicketResponse {
    #[prost(uint32, optional, tag = "1")]
    pub app_id: Option<u32>,
    #[prost(int32, optional, tag = "2", default = "2")]
    pub eresult: Option<i32>,
    #[prost(message, optional, tag = "3")]
    pub encrypted_app_ticket: Option<EncryptedAppTicket>,
}

/// An encrypted app ticket: a credential. Its bytes never appear in `Debug`
/// output and are wiped from memory when dropped.
#[derive(Clone)]
pub struct AppTicket(Zeroizing<Vec<u8>>);

impl AppTicket {
    pub fn new(bytes: Vec<u8>) -> AppTicket {
        AppTicket(Zeroizing::new(bytes))
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// Lowercase hex, the form Bethesda.net's `external-login` wants.
    pub fn to_hex(&self) -> Zeroizing<String> {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(self.0.len() * 2);
        for b in self.0.iter() {
            s.push(DIGITS[(b >> 4) as usize] as char);
            s.push(DIGITS[(b & 15) as usize] as char);
        }
        Zeroizing::new(s)
    }
}

impl std::fmt::Debug for AppTicket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AppTicket(<{} bytes>)", self.0.len())
    }
}

/// The request body for `app`, with `userdata` folded into the ticket.
pub(crate) fn request_body(app: u32, userdata: &[u8]) -> Vec<u8> {
    RequestEncryptedAppTicket {
        app_id: Some(app),
        userdata: Some(userdata.to_vec()),
    }
    .encode_to_vec()
}

/// The ticket in a 5527 body answering a request for `app`.
pub(crate) fn parse_response(app: u32, body: &[u8]) -> Result<AppTicket, SteamError> {
    let r = RequestEncryptedAppTicketResponse::decode(body)
        .map_err(|e| SteamError::Protocol(format!("bad app ticket response: {e}")))?;
    match r.eresult.unwrap_or(2) {
        1 => {}
        15 => return Err(SteamError::NotOwned { app }),
        25 | 84 => return Err(SteamError::RateLimited),
        3 => return Err(SteamError::Protocol("Steam is offline".into())),
        29 => {
            return Err(SteamError::Protocol(
                "an app ticket request is already pending".into(),
            ));
        }
        n => {
            return Err(SteamError::Protocol(format!(
                "Steam refused the app ticket: EResult {n}"
            )));
        }
    }
    if r.app_id != Some(app) {
        return Err(SteamError::Protocol(format!(
            "Steam answered an app ticket request for {app} with one for {:?}",
            r.app_id
        )));
    }
    let t = r
        .encrypted_app_ticket
        .ok_or_else(|| SteamError::Protocol("Steam sent no app ticket".into()))?;
    Ok(AppTicket::new(t.encode_to_vec()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn ticket_msg() -> EncryptedAppTicket {
        EncryptedAppTicket {
            ticket_version_no: Some(2),
            crc_encryptedticket: Some(0xDEAD_BEEF),
            cb_encrypteduserdata: Some(4),
            cb_encrypted_appownershipticket: Some(94),
            encrypted_ticket: Some(vec![0xAB; 144]),
        }
    }

    pub(crate) fn response(app: u32, eresult: i32, with_ticket: bool) -> Vec<u8> {
        RequestEncryptedAppTicketResponse {
            app_id: Some(app),
            eresult: Some(eresult),
            encrypted_app_ticket: with_ticket.then(ticket_msg),
        }
        .encode_to_vec()
    }

    #[test]
    fn an_ok_response_gives_the_serialized_ticket() {
        let t = parse_response(489830, &response(489830, 1, true)).unwrap();
        assert_eq!(t.bytes(), ticket_msg().encode_to_vec());
        assert_eq!(t.bytes().len(), 159, "the size measured live");
    }

    #[test]
    fn refusals_map_to_errors() {
        let e = |r| parse_response(489830, &response(489830, r, false)).unwrap_err();
        assert!(matches!(e(15), SteamError::NotOwned { app: 489830 }));
        assert!(matches!(e(25), SteamError::RateLimited));
        assert!(matches!(e(84), SteamError::RateLimited));
        assert!(e(3).to_string().contains("offline"));
        assert!(e(29).to_string().contains("pending"));
        assert!(e(2).to_string().contains("EResult 2"));
    }

    #[test]
    fn a_ticket_for_another_app_is_refused() {
        let e = parse_response(489830, &response(1, 1, true)).unwrap_err();
        assert!(matches!(e, SteamError::Protocol(_)), "{e}");
        let e = parse_response(489830, &response(489830, 1, false)).unwrap_err();
        assert!(e.to_string().contains("no app ticket"), "{e}");
    }

    #[test]
    fn hex_is_lowercase_and_debug_hides_the_bytes() {
        let t = AppTicket::new(vec![0xAB, 0x01, 0xFF]);
        assert_eq!(&*t.to_hex(), "ab01ff");
        let d = format!("{t:?}");
        assert_eq!(d, "AppTicket(<3 bytes>)");
        assert!(!d.contains("ab01ff"));
    }

    #[test]
    fn the_request_carries_app_and_userdata() {
        let r = RequestEncryptedAppTicket::decode(&*request_body(489830, &[0; 4])).unwrap();
        assert_eq!(r.app_id, Some(489830));
        assert_eq!(r.userdata, Some(vec![0; 4]));
    }
}
