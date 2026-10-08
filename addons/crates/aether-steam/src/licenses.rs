//! What the account owns: the licence list the CM pushes right after logon
//! (`CMsgClientLicenseList`, EMsg 780; steamroom has the constant but no
//! generated type) and the PICS package info that says which apps each
//! licence grants.
use crate::error::SteamError;
use prost::Message;
use steamroom::types::key_value::{KeyValue, KvValue};

/// `k_EMsgClientLicenseList`.
pub(crate) const EMSG_LICENSE_LIST: u32 = 780;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct LicenseList {
    #[prost(int32, optional, tag = "1")]
    pub eresult: Option<i32>,
    #[prost(message, repeated, tag = "2")]
    pub licenses: Vec<License>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct License {
    #[prost(uint32, optional, tag = "1")]
    pub package_id: Option<u32>,
}

/// `k_EMsgClientGetAppOwnershipTicket` and its response (steamroom's
/// `CLIENT_GET_APP_OWNERSHIP_TICKET` constant, 813, is not the protocol's).
pub(crate) const EMSG_OWNERSHIP_TICKET: u32 = 857;
pub(crate) const EMSG_OWNERSHIP_TICKET_RESPONSE: u32 = 858;

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct GetAppOwnershipTicket {
    #[prost(uint32, optional, tag = "1")]
    pub app_id: Option<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct GetAppOwnershipTicketResponse {
    #[prost(uint32, optional, tag = "1", default = "2")]
    pub eresult: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub app_id: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub ticket: Option<Vec<u8>>,
}

/// What an ownership-ticket answer says about `app`: `Some(true)` owned
/// (Steam issued the ticket), `Some(false)` not owned (access denied),
/// `None` nothing conclusive (another EResult, or an answer for another
/// app), so the licence list has to decide.
pub(crate) fn ownership_from_ticket(app: u32, body: &[u8]) -> Result<Option<bool>, SteamError> {
    let r = GetAppOwnershipTicketResponse::decode(body)
        .map_err(|e| SteamError::Protocol(format!("bad app ownership ticket answer: {e}")))?;
    if r.app_id != Some(app) {
        return Ok(None);
    }
    Ok(match r.eresult.unwrap_or(2) {
        1 if r.ticket.as_ref().is_some_and(|t| !t.is_empty()) => Some(true),
        15 => Some(false),
        _ => None,
    })
}

/// The package ids of a licence list body.
pub(crate) fn package_ids(body: &[u8]) -> Result<Vec<u32>, SteamError> {
    let l = LicenseList::decode(body)
        .map_err(|e| SteamError::Protocol(format!("bad licence list: {e}")))?;
    let mut ids: Vec<u32> = l.licenses.iter().filter_map(|x| x.package_id).collect();
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}

fn number(v: &KvValue) -> Option<u64> {
    match v {
        KvValue::Int32(i) => u64::try_from(*i).ok(),
        KvValue::UInt64(i) => Some(*i),
        KvValue::Int64(i) => u64::try_from(*i).ok(),
        KvValue::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Whether a package's PICS key values list `app` under `appids`. The tree
/// is either the package's own section or a root holding exactly that
/// section (keyed by the package id).
pub(crate) fn grants(kv: &KeyValue, app: u32) -> bool {
    let section = match (&kv.value, kv.get("appids")) {
        (_, Some(_)) => kv,
        (KvValue::Children(c), None) if c.len() == 1 => c.values().next().expect("one child"),
        _ => return false,
    };
    match section.get("appids").map(|a| &a.value) {
        Some(KvValue::Children(ids)) => ids
            .values()
            .any(|v| number(&v.value) == Some(u64::from(app))),
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn leaf(key: &str, value: KvValue) -> KeyValue {
        KeyValue {
            key: key.into(),
            value,
        }
    }

    fn node(key: &str, children: Vec<KeyValue>) -> KeyValue {
        let map: BTreeMap<String, KeyValue> =
            children.into_iter().map(|c| (c.key.clone(), c)).collect();
        leaf(key, KvValue::Children(map))
    }

    pub(crate) fn package(id: u32, apps: &[u32]) -> KeyValue {
        let appids = apps
            .iter()
            .enumerate()
            .map(|(i, a)| leaf(&i.to_string(), KvValue::Int32(*a as i32)))
            .collect();
        node(
            &id.to_string(),
            vec![
                leaf("packageid", KvValue::Int32(id as i32)),
                node("appids", appids),
            ],
        )
    }

    pub(crate) fn license_list(ids: &[u32]) -> Vec<u8> {
        LicenseList {
            eresult: Some(1),
            licenses: ids
                .iter()
                .map(|p| License {
                    package_id: Some(*p),
                })
                .collect(),
        }
        .encode_to_vec()
    }

    pub(crate) fn ownership_answer(app: u32, eresult: u32, ticket: &[u8]) -> Vec<u8> {
        GetAppOwnershipTicketResponse {
            eresult: Some(eresult),
            app_id: Some(app),
            ticket: Some(ticket.to_vec()),
        }
        .encode_to_vec()
    }

    #[test]
    fn an_ownership_ticket_answers_ownership() {
        let yes = ownership_answer(1746860, 1, &[1, 2, 3]);
        assert_eq!(ownership_from_ticket(1746860, &yes).unwrap(), Some(true));
        let denied = ownership_answer(1746860, 15, &[]);
        assert_eq!(
            ownership_from_ticket(1746860, &denied).unwrap(),
            Some(false)
        );
        for inconclusive in [
            ownership_answer(1746860, 1, &[]),
            ownership_answer(1746860, 84, &[]),
            ownership_answer(489830, 1, &[1]),
        ] {
            assert_eq!(ownership_from_ticket(1746860, &inconclusive).unwrap(), None);
        }
        assert!(ownership_from_ticket(1, &[0xff, 0xff]).is_err());
    }

    #[test]
    fn licence_list_gives_sorted_unique_package_ids() {
        assert_eq!(
            package_ids(&license_list(&[626104, 0, 135914, 626104])).unwrap(),
            vec![0, 135914, 626104]
        );
        assert!(package_ids(&[0xff, 0xff]).is_err());
    }

    #[test]
    fn a_package_grants_the_apps_it_lists() {
        let p = package(626104, &[1746860]);
        assert!(grants(&p, 1746860));
        assert!(!grants(&p, 489830));
        // Wrapped in a root, as the decoder may hand it over.
        let root = node("appinfo", vec![p]);
        assert!(grants(&root, 1746860));
        // App ids written as strings.
        let s = node(
            "1",
            vec![node(
                "appids",
                vec![leaf("0", KvValue::String("489830".into()))],
            )],
        );
        assert!(grants(&s, 489830));
        assert!(!grants(&leaf("x", KvValue::Int32(1)), 1));
    }
}
