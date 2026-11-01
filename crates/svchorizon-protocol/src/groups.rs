//! Minecraft TCP custom-payload codecs for Simple Voice Chat group management.
//! Packet layouts mirror the upstream 26.3 CreateGroupPacket, JoinGroupPacket,
//! LeaveGroupPacket, JoinedGroupPacket, AddGroupPacket and RemoveGroupPacket.

use crate::wire::{Reader, WireError, Writer};
use thiserror::Error;
use uuid::Uuid;

pub const CREATE_GROUP_CHANNEL: &str = "voicechat:create_group";
pub const SET_GROUP_CHANNEL: &str = "voicechat:set_group";
pub const LEAVE_GROUP_CHANNEL: &str = "voicechat:leave_group";
pub const JOINED_GROUP_CHANNEL: &str = "voicechat:joined_group";
pub const ADD_GROUP_CHANNEL: &str = "voicechat:add_group";
pub const REMOVE_GROUP_CHANNEL: &str = "voicechat:remove_group";

#[derive(Debug, Error)]
pub enum GroupPayloadError {
    #[error(transparent)]
    Wire(#[from] WireError),
    #[error(
        "group name must be 1–24 characters and cannot start with whitespace or contain control characters"
    )]
    InvalidName,
    #[error("group password exceeds the upstream length limit")]
    PasswordTooLong,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i16)]
pub enum GroupType {
    Normal = 0,
    Open = 1,
    Isolated = 2,
}

impl GroupType {
    fn from_wire(value: i16) -> Self {
        match value {
            1 => Self::Open,
            2 => Self::Isolated,
            _ => Self::Normal,
        }
    }
}

fn validate_group_name(name: &str) -> Result<(), GroupPayloadError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(GroupPayloadError::InvalidName);
    };
    if name.chars().count() > 24
        || first.is_whitespace()
        || first.is_control()
        || chars.clone().any(char::is_control)
    {
        return Err(GroupPayloadError::InvalidName);
    }
    Ok(())
}

fn write_utf(w: &mut Writer, value: &str) {
    w.utf(value);
}

pub fn encode_create_group(
    name: &str,
    password: Option<&str>,
    group_type: GroupType,
) -> Result<Vec<u8>, GroupPayloadError> {
    validate_group_name(name)?;
    if password.is_some_and(|value| value.chars().count() > 24) {
        return Err(GroupPayloadError::PasswordTooLong);
    }
    let mut w = Writer::default();
    write_utf(&mut w, name);
    w.bool(password.is_some());
    if let Some(password) = password {
        write_utf(&mut w, password);
    }
    w.raw(&(group_type as i16).to_be_bytes());
    Ok(w.data)
}

pub fn encode_join_group(
    group_id: Uuid,
    password: Option<&str>,
) -> Result<Vec<u8>, GroupPayloadError> {
    if password.is_some_and(|value| value.chars().count() > 256) {
        return Err(GroupPayloadError::PasswordTooLong);
    }
    let mut w = Writer::default();
    w.uuid(group_id);
    w.bool(password.is_some());
    if let Some(password) = password {
        write_utf(&mut w, password);
    }
    Ok(w.data)
}

pub fn encode_leave_group() -> Vec<u8> {
    Vec::new()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinedGroup {
    pub group_id: Option<Uuid>,
    pub wrong_password: bool,
}

impl JoinedGroup {
    pub fn decode(data: &[u8]) -> Result<Self, GroupPayloadError> {
        let mut r = Reader::new(data);
        let group_id = if r.bool()? { Some(r.uuid()?) } else { None };
        let wrong_password = r.bool()?;
        r.finish()?;
        Ok(Self {
            group_id,
            wrong_password,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientGroup {
    pub id: Uuid,
    pub name: String,
    pub has_password: bool,
    pub persistent: bool,
    pub hidden: bool,
    pub group_type: GroupType,
}

impl ClientGroup {
    pub fn decode(data: &[u8]) -> Result<Self, GroupPayloadError> {
        let mut r = Reader::new(data);
        let id = r.uuid()?;
        let name = r.utf(512)?;
        let has_password = r.bool()?;
        let persistent = r.bool()?;
        let hidden = r.bool()?;
        let group_type = GroupType::from_wire(i16::from_be_bytes(
            r.take(2)?.try_into().expect("length checked"),
        ));
        r.finish()?;
        Ok(Self {
            id,
            name,
            has_password,
            persistent,
            hidden,
            group_type,
        })
    }
}

pub fn decode_remove_group(data: &[u8]) -> Result<Uuid, GroupPayloadError> {
    let mut r = Reader::new(data);
    let id = r.uuid()?;
    r.finish()?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_create_group_in_official_field_order() {
        let bytes = encode_create_group("Horizon", Some("secret"), GroupType::Isolated).unwrap();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.utf(24).unwrap(), "Horizon");
        assert!(r.bool().unwrap());
        assert_eq!(r.utf(24).unwrap(), "secret");
        assert_eq!(
            i16::from_be_bytes(r.take(2).unwrap().try_into().unwrap()),
            2
        );
        r.finish().unwrap();
    }

    #[test]
    fn encodes_join_and_leave_group_payloads() {
        let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let bytes = encode_join_group(id, Some("pw")).unwrap();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.uuid().unwrap(), id);
        assert!(r.bool().unwrap());
        assert_eq!(r.utf(512).unwrap(), "pw");
        r.finish().unwrap();
        assert!(encode_leave_group().is_empty());
    }

    #[test]
    fn decodes_joined_group_response() {
        let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let mut w = Writer::default();
        w.bool(true);
        w.uuid(id);
        w.bool(false);
        assert_eq!(
            JoinedGroup::decode(&w.data).unwrap(),
            JoinedGroup {
                group_id: Some(id),
                wrong_password: false,
            }
        );
    }

    #[test]
    fn decodes_group_list_and_removal_payloads() {
        let id = Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap();
        let mut w = Writer::default();
        w.uuid(id);
        w.utf("Friends");
        w.bool(true);
        w.bool(false);
        w.bool(true);
        w.raw(&1i16.to_be_bytes());
        let group = ClientGroup::decode(&w.data).unwrap();
        assert_eq!(group.id, id);
        assert_eq!(group.name, "Friends");
        assert!(group.has_password);
        assert!(!group.persistent);
        assert!(group.hidden);
        assert_eq!(group.group_type, GroupType::Open);

        let mut remove = Writer::default();
        remove.uuid(id);
        assert_eq!(decode_remove_group(&remove.data).unwrap(), id);
    }

    #[test]
    fn enforces_upstream_join_password_limit() {
        let id = Uuid::nil();
        assert!(encode_join_group(id, Some(&"p".repeat(256))).is_ok());
        assert!(matches!(
            encode_join_group(id, Some(&"p".repeat(257))),
            Err(GroupPayloadError::PasswordTooLong)
        ));
    }

    #[test]
    fn rejects_invalid_group_names() {
        assert!(encode_create_group("", None, GroupType::Normal).is_err());
        assert!(encode_create_group(" bad", None, GroupType::Normal).is_err());
        assert!(encode_create_group("x".repeat(25).as_str(), None, GroupType::Normal).is_err());
    }
}
