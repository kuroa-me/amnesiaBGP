use crate::error::{ErrorCode, LocalLogicErrorSubcode, Result};
use crate::message::{Message, OpenMsg};
use bytes::{Buf, BytesMut};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

pub struct BgpConn {
    stream: TcpStream,
    buffer: BytesMut,
    peer: Option<OpenMsg>,
}

impl BgpConn {
    pub fn new(stream: TcpStream) -> BgpConn {
        BgpConn {
            stream,
            buffer: BytesMut::with_capacity(4096), // Longest BGP message is 4096 octets.
            peer: None,
        }
    }

    pub async fn read_message(&mut self) -> Result<Message> {
        loop {
            let frame = match Message::decode(&mut self.buffer, self.peer.as_ref()) {
                Ok(frame) => {
                    if let Message::Open(open) = frame.clone() {
                        self.peer = Some(open);
                    }
                    frame
                }
                Err(e) => {
                    if e.code == ErrorCode::LocalLogicError
                        && e.subcode == LocalLogicErrorSubcode::BufferTooShortError.into()
                    {
                        self.stream.read_buf(&mut self.buffer).await?;
                        continue;
                    }
                    return Err(e);
                }
            };
            return Ok(frame);

            // if len == 0 {
            //     while let frame = Message::decode(&mut self.buffer, self.peer.as_ref())? {
            //         yield frame
            //     }
            // }

            // while let frame = Message::decode(&mut self.buffer, self.peer.as_ref())? {
            //     if let Message::Open(open) = frame {
            //         self.peer = Some(open);
            //     }
            //     yield frame;
            // }
        }
    }
}
