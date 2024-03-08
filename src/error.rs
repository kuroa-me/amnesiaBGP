use core::any::Any;

use crate::message::NotificationMsg;

pub type BError = Box<Error>;
pub type Result<T> = std::result::Result<T, BError>;

#[derive(Debug)]
pub struct Error {
    pub code: ErrorCode,
    pub subcode: ErrorSubCode,
    pub data: Option<Box<(dyn Any + Send + Sync)>>,
}

impl Error {
    pub fn new(
        code: ErrorCode,
        subcode: ErrorSubCode,
        data: Option<Box<(dyn Any + Send + Sync)>>,
    ) -> BError {
        Box::new(Error {
            code,
            subcode,
            data,
        })
    }

    pub fn new_local(
        subcode: LocalLogicErrorSubcode,
        data: Option<Box<(dyn Any + Send + Sync)>>,
    ) -> BError {
        Error::new(ErrorCode::LocalLogicError, subcode.into(), data)
    }

    pub fn new_header(
        subcode: MsgHeaderErrorSubcode,
        data: Option<Box<(dyn Any + Send + Sync)>>,
    ) -> BError {
        Error::new(ErrorCode::MessageHeaderError, subcode.into(), data)
    }

    pub fn new_open(
        subcode: OpenMsgErrorSubcode,
        data: Option<Box<(dyn Any + Send + Sync)>>,
    ) -> BError {
        Error::new(ErrorCode::OpenMessageError, subcode.into(), data)
    }

    pub fn new_update(
        subcode: UpdateMsgErrorSubcode,
        data: Option<Box<(dyn Any + Send + Sync)>>,
    ) -> BError {
        Error::new(ErrorCode::UpdateMessageError, subcode.into(), data)
    }

    pub fn err_local<T>(
        subcode: LocalLogicErrorSubcode,
        data: Option<Box<(dyn Any + Send + Sync)>>,
    ) -> Result<T> {
        Err(Self::new_local(subcode, data))
    }

    pub fn err_header<T>(
        subcode: MsgHeaderErrorSubcode,
        data: Option<Box<(dyn Any + Send + Sync)>>,
    ) -> Result<T> {
        Err(Self::new_header(subcode, data))
    }

    pub fn err_open<T>(
        subcode: OpenMsgErrorSubcode,
        data: Option<Box<(dyn Any + Send + Sync)>>,
    ) -> Result<T> {
        Err(Self::new_open(subcode, data))
    }

    pub fn err_update<T>(
        subcode: UpdateMsgErrorSubcode,
        data: Option<Box<(dyn Any + Send + Sync)>>,
    ) -> Result<T> {
        Err(Self::new_update(subcode, data))
    }
}

impl From<octets::BufferTooShortError> for BError {
    fn from(_: octets::BufferTooShortError) -> Self {
        Error::new(
            ErrorCode::LocalLogicError,
            LocalLogicErrorSubcode::BufferTooShortError.into(),
            None,
        )
    }
}

impl From<std::io::Error> for BError {
    fn from(e: std::io::Error) -> Self {
        Error::new(
            ErrorCode::LocalLogicError,
            LocalLogicErrorSubcode::IOError.into(),
            None,
        )
    }
}

impl TryInto<NotificationMsg> for BError {
    //TODO: This self wrapping might not be correct.
    type Error = Self;

    fn try_into(self) -> std::result::Result<NotificationMsg, Self::Error> {
        if self.code == ErrorCode::LocalLogicError {
            return Err(self);
        }
        Ok(NotificationMsg {
            error_code: self.code as u8,
            error_subcode: match self.subcode {
                ErrorSubCode::MsgHeaderErrorSubcode(subcode) => subcode as u8,
                ErrorSubCode::OpenMsgErrorSubcode(subcode) => subcode as u8,
                ErrorSubCode::UpdateMsgErrorSubcode(subcode) => subcode as u8,
                _ => 0,
            },
            data: None, //TODO: Implement encode
        })
    }
}

#[derive(Debug, PartialEq)]
pub enum LocalLogicErrorSubcode {
    BufferTooShortError = 1,
    IOError = 2,
}

#[derive(Debug, PartialEq)]
pub enum MsgHeaderErrorSubcode {
    ConnectionNotSync = 1,
    BadMessageLength = 2,
    BadMessageType = 3,
}

#[derive(Debug, PartialEq)]
pub enum OpenMsgErrorSubcode {
    UnsupportedVersionNumber = 1,
    BadPeerAs = 2,
    BadBgpIdentifier = 3,
    UnsupportedOptionalParameter = 4,
    // Deprecated = 5,
    UnacceptableHoldTime = 6,
    UnSupportedCapability = 7,
}

#[derive(Debug, PartialEq)]
pub enum UpdateMsgErrorSubcode {
    MalformedAttributeList = 1,
    UnrecognizedWellKnownAttribute = 2,
    MissingWellKnownAttribute = 3,
    AttributeFlagsError = 4,
    AttributeLengthError = 5,
    InvalidOriginAttribute = 6,
    // Deprecated = 7,
    InvalidNextHopAttribute = 8,
    OptionalAttributeError = 9,
    InvalidNetworkField = 10,
    MalformedAsPath = 11,
}

#[derive(Debug, PartialEq)]
pub enum ErrorSubCode {
    LocalLogicErrorSubcode(LocalLogicErrorSubcode),
    MsgHeaderErrorSubcode(MsgHeaderErrorSubcode),
    OpenMsgErrorSubcode(OpenMsgErrorSubcode),
    UpdateMsgErrorSubcode(UpdateMsgErrorSubcode),
}

impl From<LocalLogicErrorSubcode> for ErrorSubCode {
    fn from(subcode: LocalLogicErrorSubcode) -> Self {
        ErrorSubCode::LocalLogicErrorSubcode(subcode)
    }
}

impl From<MsgHeaderErrorSubcode> for ErrorSubCode {
    fn from(subcode: MsgHeaderErrorSubcode) -> Self {
        ErrorSubCode::MsgHeaderErrorSubcode(subcode)
    }
}

impl From<OpenMsgErrorSubcode> for ErrorSubCode {
    fn from(subcode: OpenMsgErrorSubcode) -> Self {
        ErrorSubCode::OpenMsgErrorSubcode(subcode)
    }
}

impl From<UpdateMsgErrorSubcode> for ErrorSubCode {
    fn from(subcode: UpdateMsgErrorSubcode) -> Self {
        ErrorSubCode::UpdateMsgErrorSubcode(subcode)
    }
}

#[derive(Debug, PartialEq)]
pub enum ErrorCode {
    LocalLogicError = 0,
    MessageHeaderError = 1,
    OpenMessageError = 2,
    UpdateMessageError = 3,
    HoldTimerExpired = 4,
    FiniteStateMachineError = 5,
    Cease = 6,
}
