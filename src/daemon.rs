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
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
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
    let mut stream = SecureStream::server(stream, token)?;
    loop {
        let request = match protocol::read_frame::<Request>(&mut stream) {
            Ok(request) => request,
            Err(MobfsError::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
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
        let response = handle_request(request, policy).unwrap_or_else(|error| Response::Error {
            message: error.to_string(),
        });
        protocol::write_frame(&mut stream, &response)?;
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
            fs::File::create(temp)?;
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
        Request::Truncate { root, rel, size } => {
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
        }
        Request::Rename { root, from, to } => {
            let root = policy.check(&root)?;
            let from = safe_join(&root, &from)?;
            let to = safe_join(&root, &to)?;
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(from, to)?;
            Ok(Response::Ok)
        }
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
        Request::Symlink { root, rel, target } => {
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
        }
        Request::Mkdir { root, rel } => {
            let root = policy.check(&root)?;
            fs::create_dir_all(safe_join(&root, &rel)?)?;
            Ok(Response::Ok)
        }
        Request::Remove { root, rel, dir } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            if dir && path.exists() {
                fs::remove_dir_all(path)?;
            } else if path.exists() {
                fs::remove_file(path)?;
            }
            Ok(Response::Ok)
        }
        Request::Run { .. } => Err(MobfsError::Remote("run requests are streamed".to_string())),
    }
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
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| MobfsError::Remote("failed to capture stdout".to_string()))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| MobfsError::Remote("failed to capture stderr".to_string()))?;
    let stdout_handle = thread::spawn(move || read_all(&mut stdout));
    let stderr_handle = thread::spawn(move || read_all(&mut stderr));
    let status = child.wait()?;
    let stdout = stdout_handle
        .join()
        .map_err(|_| MobfsError::Remote("stdout reader panicked".to_string()))??;
    let stderr = stderr_handle
        .join()
        .map_err(|_| MobfsError::Remote("stderr reader panicked".to_string()))??;
    if !stdout.is_empty() {
        protocol::write_frame(
            stream,
            &Response::RunOutput {
                stream: RunStream::Stdout,
                data: stdout.clone(),
            },
        )?;
    }
    if !stderr.is_empty() {
        protocol::write_frame(
            stream,
            &Response::RunOutput {
                stream: RunStream::Stderr,
                data: stderr.clone(),
            },
        )?;
    }
    protocol::write_frame(
        stream,
        &Response::RunResult {
            code: status.code(),
            stdout,
            stderr,
        },
    )
}

fn read_all(reader: &mut impl Read) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    reader.read_to_end(&mut data)?;
    Ok(data)
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
            .any(|part| ignore.iter().any(|ignore| ignore == part))
        {
            continue;
        }
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(path)?;
            let target = target
                .to_str()
                .ok_or_else(|| MobfsError::InvalidPath(path.display().to_string()))?
                .to_string();
            entries.insert(
                rel,
                EntryMeta {
                    kind: EntryKind::Symlink,
                    size: target.len() as u64,
                    modified: modified_secs(&metadata),
                    sha256: Some(hex::encode(sha2::Sha256::digest(target.as_bytes()))),
                    mode: mode(&metadata),
                    link_target: Some(target),
                },
            );
        } else if metadata.is_dir() {
            entries.insert(
                rel,
                EntryMeta {
                    kind: EntryKind::Dir,
                    size: 0,
                    modified: 0,
                    sha256: None,
                    mode: mode(&metadata),
                    link_target: None,
                },
            );
        } else if metadata.is_file() {
            entries.insert(
                rel,
                EntryMeta {
                    kind: EntryKind::File,
                    size: metadata.len(),
                    modified: modified_secs(&metadata),
                    sha256: Some(local::file_sha256(path)?),
                    mode: mode(&metadata),
                    link_target: None,
                },
            );
        }
    }
    Ok(Snapshot { entries })
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
