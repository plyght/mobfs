use crate::error::{MobfsError, Result};
use crate::snapshot::Snapshot;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::TcpStream;

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Hello {
        token: String,
    },
    Snapshot {
        root: String,
        ignore: Vec<String>,
    },
    ReadFile {
        root: String,
        rel: String,
    },
    WriteFile {
        root: String,
        rel: String,
        data: Vec<u8>,
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

pub fn send(stream: &mut TcpStream, request: &Request) -> Result<Response> {
    write_frame(stream, request)?;
    let response: Response = read_frame(stream)?;
    if let Response::Error { message } = response {
        Err(MobfsError::Remote(message))
    } else {
        Ok(response)
    }
}

pub fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut TcpStream) -> Result<T> {
    let mut len = [0_u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > 128 * 1024 * 1024 {
        return Err(MobfsError::Remote("protocol frame too large".to_string()));
    }
    let mut data = vec![0_u8; len];
    stream.read_exact(&mut data)?;
    Ok(serde_json::from_slice(&data)?)
}

pub fn write_frame<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<()> {
    let data = serde_json::to_vec(value)?;
    let len = u32::try_from(data.len())
        .map_err(|_| MobfsError::Remote("protocol frame too large".to_string()))?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(&data)?;
    stream.flush()?;
    Ok(())
}
