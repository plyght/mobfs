use crate::crypto::SecureStream;
use crate::error::{MobfsError, Result};
use crate::snapshot::Snapshot;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 4;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Hello,
    Snapshot {
        root: String,
        ignore: Vec<String>,
    },
    ReadFile {
        root: String,
        rel: String,
    },
    ReadFileChunk {
        root: String,
        rel: String,
        offset: u64,
        len: u64,
    },
    WriteFile {
        root: String,
        rel: String,
        data: Vec<u8>,
    },
    WriteFileStart {
        root: String,
        rel: String,
    },
    WriteFileChunk {
        root: String,
        rel: String,
        offset: u64,
        data: Vec<u8>,
    },
    WriteFileFinish {
        root: String,
        rel: String,
    },
    Mkdir {
        root: String,
        rel: String,
    },
    Remove {
        root: String,
        rel: String,
        dir: bool,
    },
    Run {
        root: String,
        command: Vec<String>,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Hello {
        version: u32,
    },
    Snapshot(Snapshot),
    File {
        data: Vec<u8>,
    },
    FileChunk {
        data: Vec<u8>,
        eof: bool,
    },
    RunResult {
        code: Option<i32>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    Ok,
    Error {
        message: String,
    },
}

pub fn send(stream: &mut SecureStream, request: &Request) -> Result<Response> {
    write_frame(stream, request)?;
    let response: Response = read_frame(stream)?;
    if let Response::Error { message } = response {
        Err(MobfsError::Remote(message))
    } else {
        Ok(response)
    }
}

pub fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut SecureStream) -> Result<T> {
    Ok(serde_json::from_slice(&stream.read_encrypted()?)?)
}

pub fn write_frame<T: Serialize>(stream: &mut SecureStream, value: &T) -> Result<()> {
    stream.write_encrypted(&serde_json::to_vec(value)?)
}
