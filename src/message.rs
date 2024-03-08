use crate::error::{
    BError, Error, LocalLogicErrorSubcode, MsgHeaderErrorSubcode, OpenMsgErrorSubcode, Result,
    UpdateMsgErrorSubcode,
};
use bytes::{Buf, BytesMut};
use octets::Octets;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use tokio_util::codec::Decoder;

/*
    0                   1                   2                   3
    0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    |                                                               |
    +                                                               +
    |                                                               |
    +                                                               +
    |                           Marker                              |
    +                                                               +
    |                                                               |
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    |          Length               |      Type     |
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
*/
pub struct MessageHeader {
    // pub marker: [u8; 16],
    pub length: u16,
    pub msg_type: MsgType,
}

pub enum MsgType {
    Open = 1,
    Update = 2,
    Notification = 3,
    Keepalive = 4,
}

const MARKER_LEN: usize = 16;
const MARKER: [u8; MARKER_LEN] = [1; MARKER_LEN];
const MIN_MSG_LEN: usize = 19;
const MIN_OPEN_LEN: usize = 29;
const MIN_UPDATE_LEN: usize = 23;
const KEEPALIVE_LEN: usize = 19;
const MIN_NOTIFICATION_LEN: usize = 21;

const AS_TRANS: u16 = 23456;

impl MessageHeader {
    pub fn decode(buf: &mut Octets<'_>) -> Result<MessageHeader> {
        // Check if the marker is all 1s
        let marker = buf.get_bytes(MARKER_LEN)?;
        if marker.buf() != MARKER {
            return Error::err_header(MsgHeaderErrorSubcode::ConnectionNotSync, None);
        }
        let length = buf.get_u16()?;
        let msg_type = match buf.get_u8()? {
            1 => MsgType::Open,
            2 => MsgType::Update,
            3 => MsgType::Notification,
            4 => MsgType::Keepalive,
            _ => {
                return Error::err_header(MsgHeaderErrorSubcode::BadMessageType, None);
            }
        };
        match msg_type {
            MsgType::Open => {
                if length < MIN_OPEN_LEN as u16 {
                    return Error::err_header(MsgHeaderErrorSubcode::BadMessageLength, None);
                }
            }
            MsgType::Update => {
                if length < MIN_UPDATE_LEN as u16 {
                    return Error::err_header(MsgHeaderErrorSubcode::BadMessageLength, None);
                }
            }
            MsgType::Notification => {
                if length < MIN_NOTIFICATION_LEN as u16 {
                    return Error::err_header(MsgHeaderErrorSubcode::BadMessageLength, None);
                }
            }
            MsgType::Keepalive => {
                if length != KEEPALIVE_LEN as u16 {
                    return Error::err_header(MsgHeaderErrorSubcode::BadMessageLength, None);
                }
            }
        }
        Ok(MessageHeader { length, msg_type })
    }
}

/*
    0       7      15      23      31
    +-------+-------+-------+-------+
    |      AFI      | Res.  | SAFI  |
    +-------+-------+-------+-------+

AFI  - Address Family Identifier (16 bit), encoded the same way
    as in the Multiprotocol Extensions

Res. - Reserved (8 bit) field. Should be set to 0 by the sender
    and ignored by the receiver.

SAFI - Subsequent Address Family Identifier (8 bit), encoded
    the same way as in the Multiprotocol Extensions.
*/
// RFC 4760 Multiprotocol Extensions for BGP-4
#[derive(Debug, Clone)]
pub struct MpExtCap {
    pub afi: u16,
    pub safi: u8,
}

impl MpExtCap {
    pub fn decode(buf: &mut Octets<'_>) -> Result<MpExtCap> {
        let afi = buf.get_u16()?;
        buf.skip(1)?; //Skip reserved byte
        let safi = buf.get_u8()?;
        Ok(MpExtCap { afi, safi })
    }
}

// RFC 6793 BGP Support for Four-Octet Autonomous System (AS) Number
#[derive(Debug, Clone)]
pub struct FourOctetAsCap {
    pub as_num: u32,
}

impl FourOctetAsCap {
    pub fn decode(buf: &mut Octets<'_>) -> Result<FourOctetAsCap> {
        Ok(FourOctetAsCap {
            as_num: buf.get_u32()?,
        })
    }
}

/*
    RFC 4271 Optional Parameters
    0                   1
    0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-...
    |  Parm. Type   | Parm. Length  |  Parameter Value (variable)
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-...

    RFC 9072 Extended Optional Parameters Length for BGP OPEN Message
    0                   1                   2
    0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    |  Parm. Type   |        Parameter Length       |
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    ~            Parameter Value (variable)         ~
    |                                               |
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
*/
#[derive(Debug, Clone)]
pub struct OptParams {
    pub mp_ext_caps: Option<Vec<MpExtCap>>,
    pub four_octet_as_cap: Option<FourOctetAsCap>,
    //TODO: Implement other optional parameters
}

impl OptParams {
    pub fn decode(buf: &mut Octets<'_>, opt_params_len: u16, ext_op: bool) -> Result<OptParams> {
        let mut opt_params = self::OptParams {
            mp_ext_caps: None,
            four_octet_as_cap: None,
        };
        for _ in 0..opt_params_len {
            let param_type = buf.get_u8()?;
            let param_len: u16;
            if ext_op {
                param_len = buf.get_u16()?;
            } else {
                param_len = buf.get_u8()?.into();
            }

            //[Capability Codes](https://www.iana.org/assignments/capability-codes/capability-codes.xhtml)
            match param_type {
                1 => {
                    if param_len != 4 {
                        return Error::err_open(
                            OpenMsgErrorSubcode::UnSupportedCapability,
                            None, //TODO: RFC 2842, fill in after implementing the decode function
                        );
                    }
                    let mp_ext_cap = MpExtCap::decode(buf)?;
                    opt_params
                        .mp_ext_caps
                        .get_or_insert(Vec::new())
                        .push(mp_ext_cap);
                }
                63 => {
                    if param_len != 4 {
                        return Error::err_open(
                            OpenMsgErrorSubcode::UnSupportedCapability,
                            None, //TODO: RFC 6793, fill in after implementing the decode function
                        );
                    }
                    opt_params.four_octet_as_cap = Some(FourOctetAsCap::decode(buf)?);
                }
                _ => {
                    return Error::err_open(
                        OpenMsgErrorSubcode::UnsupportedOptionalParameter,
                        None,
                    );
                }
            }
        }
        Ok(opt_params)
    }
}

/*
    0                   1                   2                   3
    0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
    +-+-+-+-+-+-+-+-+
    |    Version    |
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    |     My Autonomous System      |
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    |           Hold Time           |
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    |                         BGP Identifier                        |
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    |Non-Ext OP Len.|Non-Ext OP Type|  Extended Opt. Parm. Length   | //RFC 9072
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    |                                                               |
    |             Optional Parameters (variable)                    |
    |                                                               |
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
*/
// Minimum length of 29 octets
#[derive(Debug, Clone)]
pub struct OpenMsg {
    pub version: u8,
    pub as_number: u16,
    pub hold_time: u16,
    pub bgp_identifier: u32, // bgp router-id, //TODO: Check if it is the same as the BGP Identifier of the local BGP speaker and the message is from an internal peer
    pub ext_opt_param: bool,
    pub opt_params_len: u16,
    pub opt_params: OptParams,
}

impl OpenMsg {
    pub fn decode(buf: &mut Octets<'_>) -> Result<OpenMsg> {
        let version = buf.get_u8()?;
        if version != 4 {
            return Error::err_open(OpenMsgErrorSubcode::UnsupportedVersionNumber, None);
        }
        buf.skip(3)?; //Skip padding

        let as_number = buf.get_u16()?;
        buf.skip(2)?; //Skip padding

        //TODO: Implement "unacceptable" AS number check
        let hold_time = buf.get_u16()?;
        //RFC 4271: The Hold Time MUST be either zero or at least three seconds.
        if hold_time != 0 && hold_time < 3 {
            return Error::err_open(OpenMsgErrorSubcode::UnacceptableHoldTime, None);
        }
        buf.skip(2)?; //Skip padding

        let bgp_identifier = buf.get_u32()?;
        // RFC 4271: The value of BGP Identifier field must not be all zeros or all ones.
        if bgp_identifier == 0 {
            return Error::err_open(OpenMsgErrorSubcode::BadBgpIdentifier, None);
        }

        let non_ext_op_len = buf.get_u8()?;
        let non_ext_op_type = buf.get_u8()?;
        let ext_opt_parm_len = buf.get_u16()?;

        let opt_params_len;
        let ext_opt_param;
        match non_ext_op_type {
            255 => {
                opt_params_len = ext_opt_parm_len;
                ext_opt_param = true;
            }
            _ => {
                opt_params_len = non_ext_op_len as u16;
                ext_opt_param = false;
            }
        };

        let opt_params = OptParams::decode(buf, opt_params_len, ext_opt_param)?;

        Ok(OpenMsg {
            version,
            as_number,
            hold_time,
            bgp_identifier,
            ext_opt_param,
            opt_params_len,
            opt_params,
        })
    }
}

/*
    +---------------------------+
    |   Length (1 octet)        |
    +---------------------------+
    |   Prefix (variable)       |
    +---------------------------+
*/
#[derive(Debug, Clone)]
pub struct Cidr {
    pub length: u8,
    pub prefix: IpAddr,
}

impl Cidr {
    pub fn decode(buf: &mut Octets<'_>) -> Result<Cidr> {
        let length = buf.get_u8()?;
        let padded_length = (length + 7) / 8; // Round up to the nearest byte
        let mut padded_prefix = buf.get_bytes(padded_length as usize)?.to_vec();
        if padded_length > 4 {
            // Pad the prefix with 0s for IPv6
            for _ in padded_length..16 {
                padded_prefix.push(0);
            }

            Ok(Cidr {
                length,
                prefix: IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(padded_prefix).unwrap())), //TODO: There is no way unwrap can fail. But still, handle the error.
            })
        } else {
            // Pad the prefix with 0s for IPv4
            for _ in padded_length..4 {
                padded_prefix.push(0);
            }

            Ok(Cidr {
                length,
                prefix: IpAddr::V4(Ipv4Addr::from(<[u8; 4]>::try_from(padded_prefix).unwrap())), //TODO: There is no way unwrap can fail. But still, handle the error.
            })
        }
    }
}

#[derive(Debug, Clone)]
pub struct PathAttributeFlags {
    pub optional: bool,
    pub transitive: bool,
    pub partial: bool,
    pub extended_length: bool,
}

impl PathAttributeFlags {
    pub fn decode(buf: &mut Octets<'_>) -> Result<PathAttributeFlags> {
        let flags = buf.get_u8()?;
        Ok(PathAttributeFlags {
            optional: flags & 0b10000000 != 0,
            transitive: flags & 0b01000000 != 0,
            partial: flags & 0b00100000 != 0,
            extended_length: flags & 0b00010000 != 0,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Origin {
    Igp = 0,
    Egp = 1,
    Incomplete = 2,
}

impl Origin {
    pub fn decode(buf: &mut Octets<'_>, attr_len: u16) -> Result<PathAttributeValue> {
        if attr_len != 1 {
            return Error::err_update(UpdateMsgErrorSubcode::AttributeLengthError, None);
        }
        let origin = buf.get_u8()?;
        match origin {
            0 => Ok(PathAttributeValue::Origin(Origin::Igp)),
            1 => Ok(PathAttributeValue::Origin(Origin::Egp)),
            2 => Ok(PathAttributeValue::Origin(Origin::Incomplete)),
            _ => Error::err_update(UpdateMsgErrorSubcode::InvalidOriginAttribute, None),
        }
    }
}

#[derive(Debug, Clone)]
pub enum AsPathSegmentType {
    AsSet = 1,
    AsSequence = 2,
}

impl AsPathSegmentType {
    pub fn decode(buf: &mut Octets<'_>) -> Result<AsPathSegmentType> {
        match buf.get_u8()? {
            1 => Ok(AsPathSegmentType::AsSet),
            2 => Ok(AsPathSegmentType::AsSequence),
            _ => Error::err_update(UpdateMsgErrorSubcode::MalformedAsPath, None),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AsNum(u32);

impl AsNum {
    pub fn is_mappable(&self) -> bool {
        self.0 >> 16 == 0
    }
    pub fn to_two_octets(&self) -> u16 {
        if self.is_mappable() {
            return self.clone().0 as u16;
        }
        AS_TRANS
    }
}

#[derive(Debug, Clone)]
pub struct AsPathSegment {
    pub segment_type: AsPathSegmentType, // 1 = AS_SET, 2 = AS_SEQUENCE
    pub segment_length: u8,
    pub segment_value: Vec<AsNum>,
}

impl AsPathSegment {
    pub fn decode(buf: &mut Octets<'_>, attr_len: u16) -> Result<AsPathSegment> {
        let segment_type = AsPathSegmentType::decode(buf)?;
        let segment_length = buf.get_u8()?;
        if segment_length as u16 * 2 + 2 != attr_len {
            return Error::err_update(UpdateMsgErrorSubcode::MalformedAsPath, None);
        }
        let mut segment_value = Vec::new();
        for _ in 0..segment_length {
            segment_value.push(AsNum(buf.get_u16()? as u32));
        }
        Ok(AsPathSegment {
            segment_type,
            segment_length,
            segment_value,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Aggregator {
    pub as_num: u32,
    pub ip_addr: Ipv4Addr,
}

impl Aggregator {
    pub fn decode(buf: &mut Octets<'_>, four_octets: bool) -> Result<Aggregator> {
        let as_num = if four_octets {
            buf.get_u32()?
        } else {
            buf.get_u16()? as u32
        };
        let ip_addr = Ipv4Addr::from(
            <[u8; 4]>::try_from(buf.get_bytes(4)?.to_vec()).unwrap(), //TODO: There is no way unwrap can fail. But still, handle the error.
        );
        Ok(Aggregator { as_num, ip_addr })
    }
}

#[derive(Debug, Clone)]
pub enum PathAttributeValue {
    Origin(Origin),
    AsPath(AsPathSegment),
    NextHop(Ipv4Addr),
    MultiExitDisc(u32),
    LocalPref(u32),
    AtomicAggregate(bool),
    Aggregator(Aggregator),
    Unknown(Vec<u8>),
}

impl PathAttributeValue {
    pub fn decode(
        buf: &mut Octets<'_>,
        peer: &OpenMsg,
        attr: &mut PathAttribute,
    ) -> Result<PathAttributeValue> {
        match attr.attr_type_code {
            1 => Ok(Origin::decode(buf, attr.attr_len)?.into()),
            2 => Ok(AsPathSegment::decode(buf, attr.attr_len)?.into()),
            3 => {
                if attr.attr_len != 4 {
                    return Error::err_update(UpdateMsgErrorSubcode::AttributeLengthError, None);
                }
                Ok(PathAttributeValue::NextHop(Ipv4Addr::from(
                    <[u8; 4]>::try_from(buf.get_bytes(4)?.to_vec()).unwrap(), //TODO: There is no way unwrap can fail. But still, handle the error.
                )))
            }
            4 => Ok(PathAttributeValue::MultiExitDisc(buf.get_u32()?)),
            5 => Ok(PathAttributeValue::LocalPref(buf.get_u32()?)),
            6 => Ok(PathAttributeValue::AtomicAggregate(true)),
            7 => Ok(Aggregator::decode(buf, peer.opt_params.four_octet_as_cap.is_some())?.into()),
            _ => Ok(PathAttributeValue::Unknown(
                buf.get_bytes(attr.attr_len as usize)?.to_vec(),
            )),
        }
    }
}

impl Into<PathAttributeValue> for Origin {
    fn into(self) -> PathAttributeValue {
        PathAttributeValue::Origin(self)
    }
}

impl Into<PathAttributeValue> for AsPathSegment {
    fn into(self) -> PathAttributeValue {
        PathAttributeValue::AsPath(self)
    }
}

impl Into<PathAttributeValue> for Aggregator {
    fn into(self) -> PathAttributeValue {
        PathAttributeValue::Aggregator(self)
    }
}

/*
    0                   1
    0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    |  Attr. Flags  |Attr. Type Code|
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
*/
#[derive(Debug, Clone)]
pub struct PathAttribute {
    pub attr_flags: PathAttributeFlags,
    pub attr_type_code: u8,
    pub attr_len: u16,
    pub attr_value: PathAttributeValue,
}

impl PathAttribute {
    pub fn decode(buf: &mut Octets<'_>, peer: &OpenMsg) -> Result<PathAttribute> {
        let attr_flags = PathAttributeFlags::decode(buf)?;
        let attr_type_code = buf.get_u8()?;
        let attr_len = if attr_flags.extended_length {
            buf.get_u16()?
        } else {
            buf.get_u8()?.into()
        };
        let mut attr = PathAttribute {
            attr_flags,
            attr_type_code,
            attr_len,
            attr_value: PathAttributeValue::Unknown(Vec::new()),
        };
        attr.attr_value = PathAttributeValue::decode(buf, peer, &mut attr)?;
        Ok(attr)
    }
}

/*
    +-----------------------------------------------------+
    |   Withdrawn Routes Length (2 octets)                |
    +-----------------------------------------------------+
    |   Withdrawn Routes (variable)                       |
    +-----------------------------------------------------+
    |   Total Path Attribute Length (2 octets)            |
    +-----------------------------------------------------+
    |   Path Attributes (variable)                        |
    +-----------------------------------------------------+
    |   Network Layer Reachability Information (variable) |
    +-----------------------------------------------------+
*/
#[derive(Debug, Clone)]
pub struct UpdateMsg {
    // pub withdrawn_routes_len: u16,
    pub withdrawn_routes: Vec<Cidr>,
    // pub total_path_attr_len: u16,
    pub path_attributes: Vec<PathAttribute>,
    pub nlri: Vec<(u8, IpAddr)>,
}

impl UpdateMsg {
    pub fn decode(buf: &mut Octets<'_>, peer: &OpenMsg) -> Result<UpdateMsg> {
        let mut withdrawn_routes = Vec::new();
        let mut withdrawn_routes_buf = buf.get_bytes_with_u16_length()?;
        while withdrawn_routes_buf.cap() > 0 {
            withdrawn_routes.push(Cidr::decode(&mut withdrawn_routes_buf)?);
        }

        let mut path_attributes_buf = buf.get_bytes_with_u16_length()?;
        let mut path_attributes = Vec::new();
        while path_attributes_buf.cap() > 0 {
            path_attributes.push(PathAttribute::decode(&mut path_attributes_buf, peer)?);
        }
        if path_attributes_buf.cap() != 0 {
            return Error::err_update(UpdateMsgErrorSubcode::MalformedAttributeList, None);
        }

        let mut nlri = Vec::new();
        while buf.cap() > 0 {
            let length = buf.get_u8()?;
            let prefix = Cidr::decode(buf)?;
            nlri.push((length, prefix.prefix));
        }
        if buf.cap() != 0 {
            return Error::err_update(UpdateMsgErrorSubcode::InvalidNetworkField, None);
        }

        Ok(UpdateMsg {
            withdrawn_routes,
            path_attributes,
            nlri,
        })
    }
}

#[derive(Debug, Clone)]
pub struct KeepaliveMsg {
    // empty
}

impl KeepaliveMsg {
    pub fn decode(buf: &mut Octets<'_>) -> Result<KeepaliveMsg> {
        Ok(KeepaliveMsg {})
    }
}

/*
    0                   1                   2                   3
    0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    | Error code    | Error subcode |   Data (variable)             |
    +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
*/
// Also use this as an error message
#[derive(Debug, Clone)]
pub struct NotificationMsg {
    pub error_code: u8,
    pub error_subcode: u8,
    pub data: Option<Vec<u8>>, // Message Length = 21 + Data Length
}

impl NotificationMsg {
    pub fn decode(buf: &mut Octets<'_>) -> Result<NotificationMsg> {
        Ok(NotificationMsg {
            error_code: buf.get_u8()?,
            error_subcode: buf.get_u8()?,
            data: Some(buf.get_bytes(buf.cap())?.to_vec()),
        })
    }
}

#[derive(Debug, Clone)]
pub enum Message {
    Open(OpenMsg),
    Update(UpdateMsg),
    Keepalive(KeepaliveMsg),
    Notification(NotificationMsg),
}

impl Message {
    pub fn decode(buf: &mut BytesMut, peer: Option<&OpenMsg>) -> Result<Message> {
        // Decode the message header
        if buf.len() < MIN_MSG_LEN {
            buf.reserve(MIN_MSG_LEN - buf.len());
            return Error::err_local(LocalLogicErrorSubcode::BufferTooShortError, None);
        }
        let mut oct = Octets::with_slice(buf);
        let header = MessageHeader::decode(&mut oct)?;

        // Decode the message body
        if buf.len() < header.length as usize {
            buf.reserve(header.length as usize - buf.len());
            return Error::err_local(LocalLogicErrorSubcode::BufferTooShortError, None);
        }
        let mut oct = oct.get_bytes(header.length as usize)?;

        let res = match header.msg_type {
            MsgType::Open => Ok(Message::Open(OpenMsg::decode(&mut oct)?)),
            MsgType::Update => {
                let peer = peer.ok_or(Error::new_header(
                    MsgHeaderErrorSubcode::ConnectionNotSync,
                    None,
                ))?;
                Ok(Message::Update(UpdateMsg::decode(&mut oct, peer)?))
            }
            MsgType::Keepalive => Ok(Message::Keepalive(KeepaliveMsg::decode(&mut oct)?)),
            MsgType::Notification => Ok(Message::Notification(NotificationMsg::decode(&mut oct)?)),
        };
        buf.advance(oct.off() + MIN_MSG_LEN);
        res
    }
}
