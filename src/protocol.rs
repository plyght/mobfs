use crate::crypto::SecureStream;
use crate::error::{MobfsError, Result};
use crate::snapshot::Snapshot;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 12;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Hello,
    Snapshot {
        root: String,
        ignore: Vec<String>,
    },
    Stat {
        root: String,
        rel: String,
    },
    ListDir {
        root: String,
        rel: String,
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
    ReadSmallFiles {
        root: String,
        rels: Vec<String>,
        max_file_bytes: u64,
        max_total_bytes: u64,
    },
    WriteFile {
        root: String,
        rel: String,
        data: Vec<u8>,
        mode: u32,
    },
    WriteFileStart {
        root: String,
        rel: String,
        upload_id: String,
    },
    WriteFileChunk {
        root: String,
        rel: String,
        upload_id: String,
        offset: u64,
        data: Vec<u8>,
    },
    WriteFileOffset {
        root: String,
        rel: String,
        upload_id: String,
    },
    WriteFileAt {
        root: String,
        rel: String,
        offset: u64,
        data: Vec<u8>,
    },
    WriteFileAtBinary {
        root: String,
        rel: String,
        offset: u64,
        len: u64,
    },
    WriteFileAtStream {
        root: String,
        rel: String,
        offset: u64,
        len: u64,
        op_id: Option<String>,
    },
    Truncate {
        root: String,
        rel: String,
        size: u64,
        op_id: Option<String>,
    },
    Fsync {
        root: String,
        rel: String,
    },
    Rename {
        root: String,
        from: String,
        to: String,
        op_id: Option<String>,
    },
    WriteFileFinish {
        root: String,
        rel: String,
        upload_id: String,
        sha256: String,
        mode: u32,
    },
    Symlink {
        root: String,
        rel: String,
        target: String,
        op_id: Option<String>,
    },
    SetMetadata {
        root: String,
        rel: String,
        mode: Option<u32>,
        modified: Option<i64>,
        op_id: Option<String>,
    },
    Mkdir {
        root: String,
        rel: String,
        op_id: Option<String>,
    },
    Remove {
        root: String,
        rel: String,
        dir: bool,
        op_id: Option<String>,
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
    Stat(Option<crate::snapshot::EntryMeta>),
    DirEntries(Vec<(String, crate::snapshot::EntryMeta)>),
    File {
        data: Vec<u8>,
    },
    FileChunk {
        data: Vec<u8>,
        eof: bool,
    },
    SmallFiles(Vec<(String, Vec<u8>)>),
    FileOffset(u64),
    RunOutput {
        stream: RunStream,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunStream {
    Stdout,
    Stderr,
}

pub fn send(stream: &mut SecureStream, request: &Request) -> Result<Response> {
    write_frame(stream, request)?;
    read_response(stream)
}

pub fn send_with_byte_stream(
    stream: &mut SecureStream,
    request: &Request,
    data: &[u8],
    chunk_size: usize,
) -> Result<Response> {
    write_frame(stream, request)?;
    for chunk in data.chunks(chunk_size) {
        stream.write_encrypted(chunk)?;
    }
    read_response(stream)
}

fn read_response(stream: &mut SecureStream) -> Result<Response> {
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
