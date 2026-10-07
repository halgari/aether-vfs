//! The registry overlay opcodes (15-22), answered from an attached [`RegistryHost`].

use vfs_protocol::{
    decode_reg_changed, decode_reg_create_key, decode_reg_delete_value, decode_reg_path,
    decode_reg_rename_key, decode_reg_set_value, encode_reg_changed_reply, encode_reg_lookup_reply,
    encode_reg_version_reply, OP_REG_CHANGED, OP_REG_CREATE_KEY, OP_REG_DELETE_KEY,
    OP_REG_DELETE_VALUE, OP_REG_KEY, OP_REG_LOOKUP, OP_REG_RENAME_KEY, OP_REG_SET_VALUE,
    ST_REPLY_TOO_LARGE,
};

use super::{bad, max_read_data, reply, Reply};
use crate::registry::{lookup_state, RegistryHost};

/// The registry overlay opcodes (15-22) against an attached [`RegistryHost`]. A payload that
/// does not decode, or a path that is not a canonical `\Registry\...` key path, is
/// `ST_BAD_REQUEST`; overlay errors map through [`crate::registry::reg_status`]. A `REG_KEY`
/// reply larger than an inline reply can carry (`payload_cap - 8`) is `ST_REPLY_TOO_LARGE`.
///
/// A write that succeeds has already published the director's registry generation when the
/// host returns ([`RegistryHost::publish_to`]), so its reply cannot reach the writer while
/// another process can still use a cached answer from before it.
pub(super) fn dispatch_registry(
    host: &RegistryHost,
    opcode: u32,
    payload: &[u8],
    payload_cap: u32,
) -> Reply {
    let version = |r: Result<u64, i32>| reply(r.map(encode_reg_version_reply));
    match opcode {
        OP_REG_LOOKUP => match decode_reg_path(payload) {
            Some(p) => reply(
                host.lookup(p)
                    .map(|(l, below, v)| encode_reg_lookup_reply(lookup_state(l), below, v)),
            ),
            None => bad(),
        },
        OP_REG_KEY => match decode_reg_path(payload) {
            Some(p) => match host.key_reply(p) {
                Ok(b) if b.len() > max_read_data(payload_cap) => (ST_REPLY_TOO_LARGE, Vec::new()),
                r => reply(r),
            },
            None => bad(),
        },
        OP_REG_SET_VALUE => match decode_reg_set_value(payload) {
            Some((p, name, ty, data)) => version(host.set_value(p, name, ty, data)),
            None => bad(),
        },
        OP_REG_DELETE_VALUE => match decode_reg_delete_value(payload) {
            Some((p, name)) => version(host.delete_value(p, name)),
            None => bad(),
        },
        OP_REG_CREATE_KEY => match decode_reg_create_key(payload) {
            Some((p, volatile)) => version(host.create_key(p, volatile)),
            None => bad(),
        },
        OP_REG_DELETE_KEY => match decode_reg_path(payload) {
            Some(p) => version(host.delete_key(p)),
            None => bad(),
        },
        OP_REG_RENAME_KEY => match decode_reg_rename_key(payload) {
            Some((p, leaf)) => version(host.rename_key(p, leaf)),
            None => bad(),
        },
        OP_REG_CHANGED => match decode_reg_changed(payload) {
            Some((p, subtree, since)) => reply(
                host.changed(p, subtree, since)
                    .map(|(changed, v)| encode_reg_changed_reply(changed, v)),
            ),
            None => bad(),
        },
        _ => bad(),
    }
}
