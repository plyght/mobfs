use crate::crypto::SecureStream;
use crate::error::{MobfsError, Result};
use crate::local;
use crate::protocol::{self, PROTOCOL_VERSION, Request, Response};
use crate::snapshot::{EntryKind, EntryMeta, Snapshot};
use filetime::{FileTime, set_file_mtime};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
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
        Request::WriteFile { root, rel, data } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let temp = atomic_temp_path(&path);
            fs::File::create(&temp)?.write_all(&data)?;
            fs::rename(temp, path)?;
            Ok(Response::Ok)
        }
        Request::WriteFileStart { root, rel } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let temp = atomic_temp_path(&path);
            fs::File::create(temp)?;
            Ok(Response::Ok)
        }
        Request::WriteFileChunk {
            root,
            rel,
            offset,
            data,
        } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            let temp = atomic_temp_path(&path);
            let mut file = fs::OpenOptions::new().write(true).open(temp)?;
            file.seek(SeekFrom::Start(offset))?;
            file.write_all(&data)?;
            Ok(Response::Ok)
        }
        Request::WriteFileFinish { root, rel } => {
            let root = policy.check(&root)?;
            let path = safe_join(&root, &rel)?;
            let temp = atomic_temp_path(&path);
            fs::rename(temp, path)?;
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
        Request::Run { root, command } => {
            let root = policy.check(&root)?;
            if command.is_empty() {
                return Err(MobfsError::Remote("empty command".to_string()));
            }
            let output = Command::new(&command[0])
                .args(&command[1..])
                .current_dir(safe_join(&root, "")?)
                .output()?;
            Ok(Response::RunResult {
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            })
        }
    }
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
        let metadata = item.metadata()?;
        if metadata.is_dir() {
            entries.insert(
                rel,
                EntryMeta {
                    kind: EntryKind::Dir,
                    size: 0,
                    modified: 0,
                    sha256: None,
                },
            );
        } else if metadata.is_file() {
            entries.insert(
                rel,
                EntryMeta {
                    kind: EntryKind::File,
                    size: metadata.len(),
                    modified: metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|duration| duration.as_secs() as i64)
                        .unwrap_or(0),
                    sha256: Some(local::file_sha256(path)?),
                },
            );
        }
    }
    Ok(Snapshot { entries })
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

pub fn set_mtime(path: &Path, modified: i64) -> Result<()> {
    if modified > 0 {
        set_file_mtime(path, FileTime::from_unix_time(modified, 0))?;
    }
    Ok(())
}
