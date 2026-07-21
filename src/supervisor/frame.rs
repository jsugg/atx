//! Length-prefixed JSON frame codec for the supervisor wake protocol.

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{JobId, Revision};

const MAX_FRAME_BYTES: usize = 64 * 1024;
pub(crate) const PROTOCOL_VERSION: u16 = 1;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum IpcMessage {
    Wake {
        protocol: u16,
        job_id: JobId,
        revision: Revision,
    },
    Ack {
        protocol: u16,
        job_id: JobId,
        revision: Revision,
    },
    Shutdown {
        protocol: u16,
    },
}

#[derive(Debug, Error)]
pub(crate) enum FrameError {
    #[error("IPC frame is empty or too large")]
    FrameTooLarge,
    #[error("IPC I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("IPC JSON failed: {0}")]
    Json(#[from] serde_json::Error),
}

pub(crate) fn write_frame(writer: &mut impl Write, message: &IpcMessage) -> Result<(), FrameError> {
    let body = serde_json::to_vec(message)?;
    if body.len() > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge);
    }
    let length = u32::try_from(body.len()).map_err(|_| FrameError::FrameTooLarge)?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&body)?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn read_frame(reader: &mut impl Read) -> Result<IpcMessage, FrameError> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length =
        usize::try_from(u32::from_be_bytes(length)).map_err(|_| FrameError::FrameTooLarge)?;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(FrameError::FrameTooLarge);
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    serde_json::from_slice(&body).map_err(FrameError::from)
}
