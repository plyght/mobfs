use crate::crypto::SecureStream;
use crate::error::{MobfsError, Result};
use crate::local;
use crate::protocol::{self, PROTOCOL_VERSION, Request, Response, RunStream};
use crate::snapshot::{EntryKind, EntryMeta, Snapshot};
use filetime::{FileTime, set_file_mtime};
use sha2::Digest;
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use walkdir::WalkDir;

pub fn serve(
    bind: &str,
    token: &str,
    allow_roots: Vec<PathBuf>,
    allow_any_root: bool,
) -> Result<()> {
    let listener = TcpListener::bind(bind)?;
    let policy = RootPolicy::new(allow_roots, allow_any_root)?;
    crate::ui::info("mobfsd", format!("listening on {bind}"));
    for stream in listener.incoming() {
        let token = token.to_string();
        let policy = policy.clone();
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(error) = handle_client(stream, &token, &policy) {
                        crate::ui::warn(format!("client error: {error}"));
                    }
                });
            }
            Err(error) => crate::ui::warn(format!("accept error: {error}")),
        }
    }
    Ok(())
}

#[derive(Clone)]
struct RootPolicy {
    allow_roots: Vec<PathBuf>,
    allow_any_root: bool,
}

impl RootPolicy {
    fn new(allow_roots: Vec<PathBuf>, allow_any_root: bool) -> Result<Self> {
        if allow_roots.is_empty() && !allow_any_root {
            return Err(MobfsError::Config(
                "mobfsd requires --allow-root for real use or --allow-any-root for explicit unsafe local testing".to_string(),
            ));
        }
        let mut canonical = Vec::new();
        for root in allow_roots {
            canonical.push(root.canonicalize()?);
        }
        Ok(Self {
            allow_roots: canonical,
            allow_any_root,
        })
    }

    fn check(&self, root: &str) -> Result<PathBuf> {
        let root = PathBuf::from(root).canonicalize()?;
        if self.allow_any_root
            || self
                .allow_roots
                .iter()
                .any(|allowed| root.starts_with(allowed))
        {
            return Ok(root);
        }
        Err(MobfsError::Remote(format!(
            "remote root {} is not allowed by mobfsd",
            root.display()
        )))
    }
}

fn handle_client(stream: TcpStream, token: &str, policy: &RootPolicy) -> Result<()> {
    let raw_stream = stream.try_clone()?;
    let mut stream = SecureStream::server(stream, token)?;
    loop {
        let request = match protocol::read_frame::<Request>(&mut stream) {
            Ok(request) => request,
            Err(MobfsError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if should_drop_request(&request) {
            let _ = raw_stream.shutdown(Shutdown::Both);
            return Ok(());
        }
        if let Request::Run { root, command } = request {
            if let Err(error) = handle_run(root, command, policy, &mut stream) {
                protocol::write_frame(
                    &mut stream,
                    &Response::Error {
                        message: error.to_string(),
                    },
                )?;
            }
            continue;
        }
        if let Request::WriteFileAtBinary {
            root,
            rel,
            offset,
            len,
        } = request
        {
            let response = match stream.read_encrypted() {
                Ok(data) if data.len() as u64 == len => {
                    handle_write_file_at_bytes(root, rel, offset, &data, policy)
                        .map(|()| Response::Ok)
                        .unwrap_or_else(|error| Response::Error {
                            message: error.to_string(),
                        })
                }
                Ok(_) => Response::Error {
                    message: "binary write length mismatch".to_string(),
                },
                Err(error) => Response::Error {
                    message: error.to_string(),
                },
            };
            protocol::write_frame(&mut stream, &response)?;
            continue;
        }
        if let Request::WriteFileAtStream {
            root,
            rel,
            offset,
            len,
            op_id,
        } = request
        {
            let response =
                handle_write_file_at_stream(root, rel, offset, len, op_id, policy, &mut stream)
                    .map(|()| Response::Ok)
                    .unwrap_or_else(|error| Response::Error {
                        message: error.to_string(),
                    });
            protocol::write_frame(&mut stream, &response)?;
            continue;
        }
        let response = handle_request(request, policy).unwrap_or_else(|error| Response::Error {
            message: error.to_string(),
        });
        protocol::write_frame(&mut stream, &response)?;
    }
}

fn should_drop_request(request: &Request) -> bool {
    static DROPPED: AtomicBool = AtomicBool::new(false);
    let Ok(target) = std::env::var("MOBFS_TEST_DROP_ONCE") else {
        return false;
    };
    if DROPPED.load(Ordering::SeqCst) || request_label(request) != target {
        return false;
    }
    !DROPPED.swap(true, Ordering::SeqCst)
}

fn request_label(request: &Request) -> &'static str {
    match request {
        Request::Hello => "Hello",
        Request::Snapshot { .. } => "Snapshot",
        Request::Stat { .. } => "Stat",
        Request::ListDir { .. } => "ListDir",
        Request::ReadFile { .. } => "ReadFile",
        Request::ReadFileChunk { .. } => "ReadFileChunk",
        Request::ReadSmallFiles { .. } => "ReadSmallFiles",
        Request::WriteFile { .. } => "WriteFile",
        Request::WriteFileStart { .. } => "WriteFileStart",
        Request::WriteFileChunk { .. } => "WriteFileChunk",
        Request::WriteFileOffset { .. } => "WriteFileOffset",
        Request::WriteFileAt { .. } => "WriteFileAt",
        Request::WriteFileAtBinary { .. } => "WriteFileAtBinary",
        Request::WriteFileAtStream { .. } => "WriteFileAtStream",
        Request::Truncate { .. } => "Truncate",
        Request::Fsync { .. } => "Fsync",
        Request::Rename { .. } => "Rename",
        Request::WriteFileFinish { .. } => "WriteFileFinish",
        Request::Symlink { .. } => "Symlink",
        Request::SetMetadata { .. } => "SetMetadata",
        Request::Mkdir { .. } => "Mkdir",
        Request::Remove { .. } => "Remove",
        Request::Run { .. } => "Run",
    }
}

fn handle_request(request: Request, policy: &RootPolicy) -> Result<Response> {
    match request {
        Request::Hello => Ok(Response::Hello {
            version: PROTOCOL_VERSION,
        }),
        Request::Snapshot { root, ignore } => {
            let root = policy.check(&root)?;
            Ok(Response::Snapshot(snapshot(&root, &ignore)?))
        }
        Request::Stat { root, rel } => {
            let root = policy.check(&root)?;
            Ok(Response::Stat(entry_meta_fast(&safe_join(&root, &rel)?)?))
        }
        Request::ListDir { root, rel } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            let mut entries = Vec::new();
            for item in fs::read_dir(path)? {
                let item = item?;
                let name = item
                    .file_name()
                    .to_str()
                    .ok_or_else(|| MobfsError::InvalidPath(rel.clone()))?
                    .to_string();
                if let Some(meta) = entry_meta_fast(&item.path())? {
                    entries.push((name, meta));
                }
            }
            Ok(Response::DirEntries(entries))
        }
        Request::ReadFile { root, rel } => {
            let root = policy.check(&root)?;
            Ok(Response::File {
                data: fs::read(safe_join(&root, &rel)?)?,
            })
        }
        Request::ReadFileChunk {
            root,
            rel,
            offset,
            len,
        } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            let mut file = fs::File::open(path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut data = vec![0_u8; usize::try_from(len.min(1024 * 1024)).unwrap_or(1024 * 1024)];
            let read = file.read(&mut data)?;
            data.truncate(read);
            Ok(Response::FileChunk {
                eof: read == 0
                    || read < usize::try_from(len.min(1024 * 1024)).unwrap_or(1024 * 1024),
                data,
            })
        }
        Request::ReadSmallFiles {
            root,
            rels,
            max_file_bytes,
            max_total_bytes,
        } => {
            let root = policy.check(&root)?;
            let mut files = Vec::new();
            let mut total = 0_u64;
            for rel in rels {
                let path = safe_join(&root, &rel)?;
                let Some(meta) = entry_meta_fast(&path)? else {
                    continue;
                };
                if meta.kind != EntryKind::File || meta.size > max_file_bytes {
                    continue;
                }
                total = total.saturating_add(meta.size);
                if total > max_total_bytes {
                    break;
                }
                files.push((rel, fs::read(path)?));
            }
            Ok(Response::SmallFiles(files))
        }
        Request::WriteFile {
            root,
            rel,
            data,
            mode,
        } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let temp = atomic_temp_path(&path);
            fs::File::create(&temp)?.write_all(&data)?;
            set_mode(&temp, mode)?;
            fs::rename(temp, path)?;
            Ok(Response::Ok)
        }
        Request::WriteFileStart {
            root,
            rel,
            upload_id,
        } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let temp = upload_temp_path(&path, &upload_id)?;
            if !temp.exists() {
                fs::File::create(temp)?;
            }
            Ok(Response::Ok)
        }
        Request::WriteFileChunk {
            root,
            rel,
            upload_id,
            offset,
            data,
        } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            let temp = upload_temp_path(&path, &upload_id)?;
            let mut file = fs::OpenOptions::new().write(true).open(temp)?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&data)?;
            Ok(Response::Ok)
        }
        Request::WriteFileOffset {
            root,
            rel,
            upload_id,
        } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            let temp = upload_temp_path(&path, &upload_id)?;
            let offset = fs::metadata(temp).map(|meta| meta.len()).unwrap_or(0);
            Ok(Response::FileOffset(offset))
        }
        Request::WriteFileAt {
            root,
            rel,
            offset,
            data,
        } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(path)?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&data)?;
            Ok(Response::Ok)
        }
        Request::WriteFileAtBinary { .. } | Request::WriteFileAtStream { .. } => Err(
            MobfsError::Remote("binary write requests are streamed".to_string()),
        ),
        Request::Truncate {
            root,
            rel,
            size,
            op_id,
        } => once(policy, &root, op_id.as_deref(), || {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(false)
                .open(path)?;
            file.set_len(size)?;
            Ok(Response::Ok)
        }),
        Request::Fsync { root, rel } => {
            let root = policy.check(&root)?;
            fs::OpenOptions::new()
                .read(true)
                .open(safe_join(&root, &rel)?)?
                .sync_all()?;
            Ok(Response::Ok)
        }
        Request::Rename {
            root,
            from,
            to,
            op_id,
        } => once(policy, &root, op_id.as_deref(), || {
            let root = policy.check(&root)?;
            let from = safe_join(&root, &from)?;
            let to = safe_join(&root, &to)?;
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            if from.exists() {
                fs::rename(from, to)?;
            }
            Ok(Response::Ok)
        }),
        Request::WriteFileFinish {
            root,
            rel,
            upload_id,
            sha256,
            mode,
        } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            let temp = upload_temp_path(&path, &upload_id)?;
            let actual = local::file_sha256(&temp)?;
            if actual != sha256 {
                let _ = fs::remove_file(&temp);
                return Err(MobfsError::Remote("upload checksum mismatch".to_string()));
            }
            set_mode(&temp, mode)?;
            fs::rename(temp, path)?;
            Ok(Response::Ok)
        }
        Request::Symlink {
            root,
            rel,
            target,
            op_id,
        } => once(policy, &root, op_id.as_deref(), || {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let _ = fs::remove_file(&path);
            #[cfg(unix)]
            symlink(target, path)?;
            #[cfg(not(unix))]
            return Err(MobfsError::Remote(
                "symlinks are not supported on this platform".to_string(),
            ));
            Ok(Response::Ok)
        }),
        Request::SetMetadata {
            root,
            rel,
            mode,
            modified,
            op_id,
        } => once(policy, &root, op_id.as_deref(), || {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            if let Some(mode) = mode {
                set_mode(&path, mode)?;
            }
            if let Some(modified) = modified {
                set_mtime(&path, modified)?;
            }
            Ok(Response::Ok)
        }),
        Request::Mkdir { root, rel, op_id } => once(policy, &root, op_id.as_deref(), || {
            let root = policy.check(&root)?;
            fs::create_dir_all(safe_join(&root, &rel)?)?;
            Ok(Response::Ok)
        }),
        Request::Remove {
            root,
            rel,
            dir,
            op_id,
        } => once(policy, &root, op_id.as_deref(), || {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            if dir && path.exists() {
                fs::remove_dir_all(path)?;
            } else if path.exists() {
                fs::remove_file(path)?;
            }
            Ok(Response::Ok)
        }),
        Request::Run { .. } => Err(MobfsError::Remote("run requests are streamed".to_string())),
    }
}

fn handle_write_file_at_bytes(
    root: String,
    rel: String,
    offset: u64,
    data: &[u8],
    policy: &RootPolicy,
) -> Result<()> {
    let root = policy.check(&root)?;
    let path = safe_join(&root, &rel)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    file.write_all(data)?;
    Ok(())
}

fn handle_write_file_at_stream(
    root: String,
    rel: String,
    offset: u64,
    len: u64,
    op_id: Option<String>,
    policy: &RootPolicy,
    stream: &mut SecureStream,
) -> Result<()> {
    if let Some(op_id) = op_id.as_deref()
        && op_done(policy, &root, op_id)?
    {
        drain_stream_bytes(stream, len)?;
        return Ok(());
    }
    let root_path = policy.check(&root)?;
    let path = safe_join(&root_path, &rel)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut written = 0_u64;
    while written < len {
        let data = stream.read_encrypted()?;
        written = written.saturating_add(data.len() as u64);
        if written > len {
            return Err(MobfsError::Remote(
                "binary write length mismatch".to_string(),
            ));
        }
        file.write_all(&data)?;
    }
    if let Some(op_id) = op_id.as_deref() {
        mark_op_done(policy, &root, op_id)?;
    }
    Ok(())
}

fn drain_stream_bytes(stream: &mut SecureStream, len: u64) -> Result<()> {
    let mut read = 0_u64;
    while read < len {
        let data = stream.read_encrypted()?;
        read = read.saturating_add(data.len() as u64);
        if read > len {
            return Err(MobfsError::Remote(
                "binary write length mismatch".to_string(),
            ));
        }
    }
    Ok(())
}

fn once(
    policy: &RootPolicy,
    root: &str,
    op_id: Option<&str>,
    action: impl FnOnce() -> Result<Response>,
) -> Result<Response> {
    if let Some(op_id) = op_id
        && op_done(policy, root, op_id)?
    {
        return Ok(Response::Ok);
    }
    let value = action()?;
    if let Some(op_id) = op_id {
        mark_op_done(policy, root, op_id)?;
    }
    Ok(value)
}

fn op_done(policy: &RootPolicy, root: &str, op_id: &str) -> Result<bool> {
    Ok(op_marker_path(policy, root, op_id)?.exists())
}

fn mark_op_done(policy: &RootPolicy, root: &str, op_id: &str) -> Result<()> {
    let marker = op_marker_path(policy, root, op_id)?;
    if let Some(parent) = marker.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(marker, b"done")?;
    Ok(())
}

fn op_marker_path(policy: &RootPolicy, root: &str, op_id: &str) -> Result<PathBuf> {
    let root = policy.check(root)?;
    let root_hash = hex::encode(sha2::Sha256::digest(root.to_string_lossy().as_bytes()));
    let safe = op_id
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .collect::<String>();
    Ok(std::env::temp_dir()
        .join("mobfsd")
        .join("ops")
        .join(root_hash)
        .join(safe))
}

fn handle_run(
    root: String,
    command: Vec<String>,
    policy: &RootPolicy,
    stream: &mut SecureStream,
) -> Result<()> {
    let root = policy.check(&root)?;
    if command.is_empty() {
        return Err(MobfsError::Remote("empty command".to_string()));
    }
    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .current_dir(safe_join(&root, "")?)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| MobfsError::Remote("failed to capture stdout".to_string()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| MobfsError::Remote("failed to capture stderr".to_string()))?;
    let (tx, rx) = mpsc::channel();
    spawn_output_reader(stdout, RunStream::Stdout, tx.clone());
    spawn_output_reader(stderr, RunStream::Stderr, tx);
    let mut all_stdout = Vec::new();
    let mut all_stderr = Vec::new();
    let status = loop {
        while let Ok((run_stream, data)) = rx.try_recv() {
            match run_stream {
                RunStream::Stdout => all_stdout.extend_from_slice(&data),
                RunStream::Stderr => all_stderr.extend_from_slice(&data),
            }
            protocol::write_frame(
                stream,
                &Response::RunOutput {
                    stream: run_stream,
                    data,
                },
            )?;
        }
        if let Some(status) = child.try_wait()? {
            while let Ok((run_stream, data)) = rx.recv_timeout(Duration::from_millis(10)) {
                match run_stream {
                    RunStream::Stdout => all_stdout.extend_from_slice(&data),
                    RunStream::Stderr => all_stderr.extend_from_slice(&data),
                }
                protocol::write_frame(
                    stream,
                    &Response::RunOutput {
                        stream: run_stream,
                        data,
                    },
                )?;
            }
            break status;
        }
        thread::sleep(Duration::from_millis(10));
    };
    protocol::write_frame(
        stream,
        &Response::RunResult {
            code: status.code(),
            stdout: all_stdout,
            stderr: all_stderr,
        },
    )
}

fn spawn_output_reader(
    mut reader: impl Read + Send + 'static,
    stream: RunStream,
    tx: mpsc::Sender<(RunStream, Vec<u8>)>,
) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 16 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if tx.send((stream.clone(), buffer[..read].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn snapshot(root: &Path, ignore: &[String]) -> Result<Snapshot> {
    let root = root.to_path_buf();
    let mut entries = BTreeMap::new();
    for item in WalkDir::new(&root).into_iter() {
        let item = item?;
        let path = item.path();
        if path == root {
            continue;
        }
        let rel = relative_path(&root, path)?;
        if rel
            .split('/')
            .any(|part| crate::local::should_ignore_part(part, ignore))
        {
            continue;
        }
        if let Some(meta) = entry_meta(path)? {
            entries.insert(rel, meta);
        }
    }
    Ok(Snapshot { entries })
}

fn entry_meta(path: &Path) -> Result<Option<EntryMeta>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        let target = target
            .to_str()
            .ok_or_else(|| MobfsError::InvalidPath(path.display().to_string()))?
            .to_string();
        Ok(Some(EntryMeta {
            kind: EntryKind::Symlink,
            size: target.len() as u64,
            modified: modified_secs(&metadata),
            sha256: Some(hex::encode(sha2::Sha256::digest(target.as_bytes()))),
            mode: mode(&metadata),
            link_target: Some(target),
        }))
    } else if metadata.is_dir() {
        Ok(Some(EntryMeta {
            kind: EntryKind::Dir,
            size: 0,
            modified: 0,
            sha256: None,
            mode: mode(&metadata),
            link_target: None,
        }))
    } else if metadata.is_file() {
        Ok(Some(EntryMeta {
            kind: EntryKind::File,
            size: metadata.len(),
            modified: modified_secs(&metadata),
            sha256: Some(local::file_sha256(path)?),
            mode: mode(&metadata),
            link_target: None,
        }))
    } else {
        Ok(None)
    }
}

fn entry_meta_fast(path: &Path) -> Result<Option<EntryMeta>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        let target = fs::read_link(path)?;
        let target = target
            .to_str()
            .ok_or_else(|| MobfsError::InvalidPath(path.display().to_string()))?
            .to_string();
        Ok(Some(EntryMeta {
            kind: EntryKind::Symlink,
            size: target.len() as u64,
            modified: modified_secs(&metadata),
            sha256: None,
            mode: mode(&metadata),
            link_target: Some(target),
        }))
    } else if metadata.is_dir() {
        Ok(Some(EntryMeta {
            kind: EntryKind::Dir,
            size: 0,
            modified: 0,
            sha256: None,
            mode: mode(&metadata),
            link_target: None,
        }))
    } else if metadata.is_file() {
        Ok(Some(EntryMeta {
            kind: EntryKind::File,
            size: metadata.len(),
            modified: modified_secs(&metadata),
            sha256: None,
            mode: mode(&metadata),
            link_target: None,
        }))
    } else {
        Ok(None)
    }
}

fn upload_temp_path(path: &Path, upload_id: &str) -> Result<PathBuf> {
    if !upload_id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(MobfsError::InvalidPath(upload_id.to_string()));
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    Ok(path.with_file_name(format!(".{name}.mobfs-upload-{upload_id}")))
}

fn atomic_temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    path.with_file_name(format!(".{name}.mobfs-tmp-{}", std::process::id()))
}

fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .map_err(|_| MobfsError::InvalidPath(path.display().to_string()))?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(value) => parts.push(
                value
                    .to_str()
                    .ok_or_else(|| MobfsError::InvalidPath(path.display().to_string()))?
                    .to_string(),
            ),
            _ => return Err(MobfsError::InvalidPath(path.display().to_string())),
        }
    }
    Ok(parts.join("/"))
}

fn safe_join(root: &Path, rel: &str) -> Result<PathBuf> {
    let mut path = root.to_path_buf();
    for component in Path::new(rel).components() {
        match component {
            Component::Normal(value) => path.push(value),
            _ => return Err(MobfsError::InvalidPath(rel.to_string())),
        }
    }
    if !path.starts_with(root) {
        return Err(MobfsError::InvalidPath(rel.to_string()));
    }
    Ok(path)
}

fn mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        metadata.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        0
    }
}

fn modified_secs(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

pub fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    if mode != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        let _ = mode;
    }
    Ok(())
}

pub fn set_mtime(path: &Path, modified: i64) -> Result<()> {
    if modified > 0 {
        set_file_mtime(path, FileTime::from_unix_time(modified, 0))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_path_traversal() {
        let root = Path::new("/tmp/mobfs-root");
        assert!(safe_join(root, "src/lib.rs").is_ok());
        assert!(safe_join(root, "../secret").is_err());
        assert!(safe_join(root, "/tmp/secret").is_err());
        assert!(safe_join(root, "a/../../secret").is_err());
    }
}
