use crate::config::AppConfig;
use crate::error::{MobfsError, Result};
use crate::snapshot::{EntryKind, EntryMeta};
use crate::vfs::{self, Errno, MountOptions, ROOT_INO, Vfs, VfsResult};
use async_trait::async_trait;
use nfsserve::nfs::{
    FSF_CANSETTIME, FSF_HOMOGENEOUS, FSF_SYMLINK, fattr3, fileid3, filename3, fsinfo3, ftype3,
    nfspath3, nfsstat3, nfsstring, nfstime3, post_op_attr, sattr3, set_mode3, set_mtime, set_size3,
    specdata3,
};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const NFS_WRITE_HANDLE: u64 = 1 << 63;
const FSID: u64 = 0x6d6f_6266_7331;

pub struct MobfsNfs {
    vfs: Arc<Vfs>,
}

async fn blocking<T: Send + 'static>(
    action: impl FnOnce() -> VfsResult<T> + Send + 'static,
) -> std::result::Result<T, nfsstat3> {
    match tokio::task::spawn_blocking(action).await {
        Ok(result) => result.map_err(nfs_error),
        Err(_) => Err(nfsstat3::NFS3ERR_SERVERFAULT),
    }
}

fn nfs_error(error: Errno) -> nfsstat3 {
    match error.0 {
        libc::ENOENT => nfsstat3::NFS3ERR_NOENT,
        libc::EACCES => nfsstat3::NFS3ERR_ACCES,
        libc::EEXIST => nfsstat3::NFS3ERR_EXIST,
        libc::ENOTEMPTY => nfsstat3::NFS3ERR_NOTEMPTY,
        libc::EISDIR => nfsstat3::NFS3ERR_ISDIR,
        libc::ENOTDIR => nfsstat3::NFS3ERR_NOTDIR,
        libc::ENOSPC => nfsstat3::NFS3ERR_NOSPC,
        libc::EROFS => nfsstat3::NFS3ERR_ROFS,
        libc::ENAMETOOLONG => nfsstat3::NFS3ERR_NAMETOOLONG,
        libc::EINVAL => nfsstat3::NFS3ERR_INVAL,
        libc::ESTALE => nfsstat3::NFS3ERR_STALE,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

fn name(filename: &filename3) -> std::result::Result<String, nfsstat3> {
    std::str::from_utf8(&filename.0)
        .map(str::to_string)
        .map_err(|_| nfsstat3::NFS3ERR_INVAL)
}

fn fattr(vfs: &Vfs, ino: u64, meta: &EntryMeta) -> fattr3 {
    let time = nfstime3 {
        seconds: meta.modified.clamp(0, i64::from(u32::MAX)) as u32,
        nseconds: (vfs.data_version(ino) % 1_000_000_000) as u32,
    };
    let mode = meta.mode & 0o7777;
    fattr3 {
        ftype: match meta.kind {
            EntryKind::File => ftype3::NF3REG,
            EntryKind::Dir => ftype3::NF3DIR,
            EntryKind::Symlink => ftype3::NF3LNK,
        },
        mode: if mode != 0 {
            mode
        } else {
            match meta.kind {
                EntryKind::File => 0o644,
                EntryKind::Dir => 0o755,
                EntryKind::Symlink => 0o777,
            }
        },
        nlink: if meta.kind == EntryKind::Dir { 2 } else { 1 },
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        size: meta.size,
        used: meta.size,
        rdev: specdata3::default(),
        fsid: FSID,
        fileid: ino,
        atime: time,
        mtime: time,
        ctime: time,
    }
}

#[async_trait]
impl NFSFileSystem for MobfsNfs {
    fn capabilities(&self) -> VFSCapabilities {
        VFSCapabilities::ReadWrite
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_INO
    }

    async fn fsinfo(&self, root_fileid: fileid3) -> std::result::Result<fsinfo3, nfsstat3> {
        let obj_attributes = match self.getattr(root_fileid).await {
            Ok(attr) => post_op_attr::attributes(attr),
            Err(_) => post_op_attr::Void,
        };
        let block = vfs::BLOCK_SIZE as u32;
        Ok(fsinfo3 {
            obj_attributes,
            rtmax: block,
            rtpref: block,
            rtmult: 4096,
            wtmax: block,
            wtpref: block,
            wtmult: 4096,
            dtpref: 64 * 1024,
            maxfilesize: u64::MAX / 2,
            time_delta: nfstime3 {
                seconds: 1,
                nseconds: 0,
            },
            properties: FSF_SYMLINK | FSF_HOMOGENEOUS | FSF_CANSETTIME,
        })
    }

    async fn lookup(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> std::result::Result<fileid3, nfsstat3> {
        let name = name(filename)?;
        match name.as_str() {
            "." => return Ok(dirid),
            ".." => return Ok(self.vfs.parent_of(dirid)),
            _ => {}
        }
        if let Some(result) = self.vfs.lookup_fast(dirid, &name) {
            return result.map(|(ino, _)| ino).map_err(nfs_error);
        }
        let vfs = self.vfs.clone();
        blocking(move || vfs.lookup(dirid, &name).map(|(ino, _)| ino)).await
    }

    async fn getattr(&self, id: fileid3) -> std::result::Result<fattr3, nfsstat3> {
        if let Some(result) = self.vfs.getattr_fast(id) {
            return result
                .map(|meta| fattr(&self.vfs, id, &meta))
                .map_err(nfs_error);
        }
        let vfs = self.vfs.clone();
        blocking(move || vfs.getattr(id).map(|meta| fattr(&vfs, id, &meta))).await
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> std::result::Result<fattr3, nfsstat3> {
        let mode = match setattr.mode {
            set_mode3::mode(mode) => Some(mode),
            set_mode3::Void => None,
        };
        let size = match setattr.size {
            set_size3::size(size) => Some(size),
            set_size3::Void => None,
        };
        let modified = match setattr.mtime {
            set_mtime::SET_TO_CLIENT_TIME(time) => Some(i64::from(time.seconds)),
            set_mtime::SET_TO_SERVER_TIME => Some(vfs::unix_now_secs()),
            set_mtime::DONT_CHANGE => None,
        };
        let vfs = self.vfs.clone();
        blocking(move || {
            vfs.setattr(id, mode, size, modified)
                .map(|meta| fattr(&vfs, id, &meta))
        })
        .await
    }

    async fn read(
        &self,
        id: fileid3,
        offset: u64,
        count: u32,
    ) -> std::result::Result<(Vec<u8>, bool), nfsstat3> {
        let vfs = self.vfs.clone();
        blocking(move || {
            let data = vfs.read(id, offset, count)?.as_slice().to_vec();
            let size = vfs.getattr(id).map(|meta| meta.size).unwrap_or(0);
            let eof = offset.saturating_add(data.len() as u64) >= size;
            Ok((data, eof))
        })
        .await
    }

    async fn write(
        &self,
        id: fileid3,
        offset: u64,
        data: &[u8],
    ) -> std::result::Result<fattr3, nfsstat3> {
        let vfs = self.vfs.clone();
        let data = data.to_vec();
        blocking(move || {
            vfs.write(id, NFS_WRITE_HANDLE | id, offset, &data)?;
            vfs.getattr(id).map(|meta| fattr(&vfs, id, &meta))
        })
        .await
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        attr: sattr3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        let name = name(filename)?;
        let mode = match attr.mode {
            set_mode3::mode(mode) => mode,
            set_mode3::Void => 0o644,
        };
        let vfs = self.vfs.clone();
        blocking(move || {
            vfs.create(dirid, &name, mode, false)
                .map(|(ino, meta)| (ino, fattr(&vfs, ino, &meta)))
        })
        .await
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> std::result::Result<fileid3, nfsstat3> {
        let name = name(filename)?;
        let vfs = self.vfs.clone();
        blocking(move || vfs.create(dirid, &name, 0o644, true).map(|(ino, _)| ino)).await
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        let name = name(dirname)?;
        let vfs = self.vfs.clone();
        blocking(move || {
            vfs.mkdir(dirid, &name, 0o755)
                .map(|(ino, meta)| (ino, fattr(&vfs, ino, &meta)))
        })
        .await
    }

    async fn remove(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> std::result::Result<(), nfsstat3> {
        let name = name(filename)?;
        let vfs = self.vfs.clone();
        blocking(move || vfs.remove(dirid, &name, None)).await
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> std::result::Result<(), nfsstat3> {
        let from = name(from_filename)?;
        let to = name(to_filename)?;
        let vfs = self.vfs.clone();
        blocking(move || vfs.rename(from_dirid, &from, to_dirid, &to, false)).await
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> std::result::Result<ReadDirResult, nfsstat3> {
        let vfs = self.vfs.clone();
        blocking(move || {
            let mut items = vfs.list_dir(dirid)?;
            items.sort_by_key(|(ino, _, _)| *ino);
            let mut remaining = items
                .into_iter()
                .filter(|(ino, _, _)| *ino > start_after)
                .peekable();
            let mut entries = Vec::new();
            while entries.len() < max_entries {
                let Some((ino, name, meta)) = remaining.next() else {
                    break;
                };
                entries.push(DirEntry {
                    fileid: ino,
                    name: nfsstring(name.into_bytes()),
                    attr: fattr(&vfs, ino, &meta),
                });
            }
            let end = remaining.peek().is_none();
            Ok(ReadDirResult { entries, end })
        })
        .await
    }

    async fn symlink(
        &self,
        dirid: fileid3,
        linkname: &filename3,
        symlink: &nfspath3,
        _attr: &sattr3,
    ) -> std::result::Result<(fileid3, fattr3), nfsstat3> {
        let name = name(linkname)?;
        let target = std::str::from_utf8(&symlink.0)
            .map_err(|_| nfsstat3::NFS3ERR_INVAL)?
            .to_string();
        let vfs = self.vfs.clone();
        blocking(move || {
            vfs.symlink(dirid, &name, &target)
                .map(|(ino, meta)| (ino, fattr(&vfs, ino, &meta)))
        })
        .await
    }

    async fn readlink(&self, id: fileid3) -> std::result::Result<nfspath3, nfsstat3> {
        let vfs = self.vfs.clone();
        blocking(move || {
            vfs.readlink(id)
                .map(|target| nfsstring(target.into_bytes()))
        })
        .await
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .max_blocking_threads(64)
        .thread_name("mobfs-nfs")
        .enable_all()
        .build()?)
}

fn start_server(runtime: &tokio::runtime::Runtime, vfs: Arc<Vfs>, listen: &str) -> Result<u16> {
    let listener = runtime
        .block_on(NFSTcpListener::bind(listen, MobfsNfs { vfs }))
        .map_err(|error| MobfsError::Remote(format!("failed to start NFS server: {error}")))?;
    let port = listener.get_listen_port();
    runtime.spawn(async move {
        if let Err(error) = listener.handle_forever().await {
            crate::ui::warn(format!("NFS server stopped: {error}"));
        }
    });
    Ok(port)
}

pub fn serve(config: AppConfig, listen: &str, options: MountOptions) -> Result<()> {
    let vfs = Vfs::start(config, &options)?;
    let runtime = runtime()?;
    let port = start_server(&runtime, vfs.clone(), listen)?;
    crate::ui::ok(format!("serving NFSv3 on port {port}"));
    println!("{port}");
    vfs::install_signal_handlers();
    while !vfs::shutdown_requested() {
        std::thread::sleep(Duration::from_millis(200));
    }
    vfs.shutdown();
    Ok(())
}

pub fn mount(config: AppConfig, mountpoint: PathBuf, options: MountOptions) -> Result<()> {
    vfs::prepare_mountpoint(&mountpoint)?;
    let vfs = Vfs::start(config, &options)?;
    let runtime = runtime()?;
    let port = start_server(&runtime, vfs.clone(), "127.0.0.1:0")?;
    let elevated = mount_nfs(port, &mountpoint)?;
    vfs::install_signal_handlers();
    crate::ui::ok(format!("mounted {}", mountpoint.display()));
    if options.open_when_ready {
        let path = mountpoint.clone();
        std::thread::spawn(move || {
            let _ = crate::sync::open_path(&path);
        });
    }
    loop {
        if vfs::shutdown_requested() {
            crate::ui::info("unmounting", mountpoint.display().to_string());
            vfs.flush_all_buffers();
            unmount_nfs(&mountpoint, elevated);
            break;
        }
        if !vfs::is_mounted(&mountpoint) {
            crate::ui::info("unmounted", mountpoint.display().to_string());
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    vfs.shutdown();
    runtime.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

fn mount_command(port: u16, mountpoint: &Path) -> (String, Vec<String>) {
    if cfg!(target_os = "macos") {
        (
            "/sbin/mount_nfs".to_string(),
            vec![
                "-o".to_string(),
                format!(
                    "vers=3,tcp,port={port},mountport={port},locallocks,noresvport,nonegnamecache,rsize=1048576,wsize=1048576,readahead=32,actimeo=1,soft,intr,timeo=600,retrans=2"
                ),
                "localhost:/".to_string(),
                mountpoint.display().to_string(),
            ],
        )
    } else {
        (
            "mount".to_string(),
            vec![
                "-t".to_string(),
                "nfs".to_string(),
                "-o".to_string(),
                format!(
                    "vers=3,proto=tcp,port={port},mountport={port},mountproto=tcp,nolock,actimeo=1,rsize=1048576,wsize=1048576,soft,timeo=600,retrans=2"
                ),
                "127.0.0.1:/".to_string(),
                mountpoint.display().to_string(),
            ],
        )
    }
}

fn mount_nfs(port: u16, mountpoint: &Path) -> Result<bool> {
    let (program, args) = mount_command(port, mountpoint);
    let output = Command::new(&program)
        .args(&args)
        .stdin(Stdio::null())
        .output()?;
    if output.status.success() {
        return Ok(false);
    }
    let message = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let is_root = unsafe { libc::geteuid() } == 0;
    if !is_root && std::io::stdin().is_terminal() {
        crate::ui::warn(format!(
            "{program} needs administrator rights here ({message}); asking sudo to mount"
        ));
        let status = Command::new("sudo").arg(&program).args(&args).status()?;
        if status.success() {
            return Ok(true);
        }
    }
    let hint = if cfg!(target_os = "macos") {
        "Rerun in a terminal so sudo can prompt, or use `--backend fuse` with macFUSE"
    } else {
        "On Linux the NFS backend needs root and the nfs-common package; the FUSE backend (`--backend fuse`) is usually the better choice"
    };
    Err(MobfsError::Remote(format!(
        "failed to mount the MobFS NFS volume with {program}: {message}. {hint}"
    )))
}

fn unmount_nfs(mountpoint: &Path, elevated: bool) {
    let attempts: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["umount"], &["diskutil", "unmount"], &["umount", "-f"]]
    } else {
        &[&["umount"], &["umount", "-f", "-l"]]
    };
    for attempt in attempts {
        let mut command = if elevated {
            let mut command = Command::new("sudo");
            command.args(*attempt);
            command
        } else {
            let mut command = Command::new(attempt[0]);
            command.args(&attempt[1..]);
            command
        };
        let ok = command
            .arg(mountpoint)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if ok {
            return;
        }
    }
    crate::ui::warn(format!(
        "could not unmount {}; run `umount -f {}`",
        mountpoint.display(),
        mountpoint.display()
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::config_from_remote;

    fn nfs_name(value: &str) -> filename3 {
        nfsstring(value.as_bytes().to_vec())
    }

    fn start_daemon(root: &Path) -> u16 {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let root = root.to_path_buf();
        std::thread::spawn(move || {
            let _ =
                crate::daemon::serve(&format!("127.0.0.1:{port}"), "nfs-test", vec![root], false);
        });
        for _ in 0..100 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        port
    }

    #[test]
    fn nfs_adapter_handles_file_lifecycle() {
        let temp = tempfile::TempDir::new().unwrap();
        let remote = temp.path().join("remote");
        std::fs::create_dir_all(remote.join("media")).unwrap();
        std::fs::write(
            remote.join("media/clip.mov"),
            vec![7_u8; 3 * 1024 * 1024 + 11],
        )
        .unwrap();
        let port = start_daemon(&remote);
        let config = config_from_remote(
            format!("127.0.0.1:{}", remote.display()),
            &temp.path().join("mnt"),
            port,
            Some("nfs-test".to_string()),
            false,
        )
        .unwrap();
        let vfs = Vfs::start(config, &MountOptions::default()).unwrap();
        let fs = MobfsNfs { vfs: vfs.clone() };
        let runtime = runtime().unwrap();
        runtime.block_on(async {
            let root = fs.root_dir();
            let media = fs.lookup(root, &nfs_name("media")).await.unwrap();
            assert_eq!(fs.lookup(media, &nfs_name("..")).await.unwrap(), root);
            let clip = fs.lookup(media, &nfs_name("clip.mov")).await.unwrap();
            let attr = fs.getattr(clip).await.unwrap();
            assert_eq!(attr.size, 3 * 1024 * 1024 + 11);
            let (tail, eof) = fs.read(clip, 3 * 1024 * 1024, 100).await.unwrap();
            assert_eq!(tail, vec![7_u8; 11]);
            assert!(eof);

            let (doc, _) = fs
                .create(media, &nfs_name("draft.txt"), sattr3::default())
                .await
                .unwrap();
            fs.write(doc, 6, b"world").await.unwrap();
            fs.write(doc, 0, b"hello ").await.unwrap();
            fs.write(doc, 11, b"!").await.unwrap();
            let (text, _) = fs.read(doc, 0, 100).await.unwrap();
            assert_eq!(text, b"hello world!");
            assert_eq!(
                std::fs::read(remote.join("media/draft.txt")).unwrap(),
                b"hello world!"
            );
            assert!(matches!(
                fs.create_exclusive(media, &nfs_name("draft.txt")).await,
                Err(nfsstat3::NFS3ERR_EXIST)
            ));

            fs.rename(media, &nfs_name("draft.txt"), root, &nfs_name("final.txt"))
                .await
                .unwrap();
            assert!(remote.join("final.txt").exists());
            assert_eq!(fs.lookup(root, &nfs_name("final.txt")).await.unwrap(), doc);
            assert!(fs.lookup(media, &nfs_name("draft.txt")).await.is_err());

            let truncate = sattr3 {
                size: set_size3::size(5),
                ..Default::default()
            };
            assert_eq!(fs.setattr(doc, truncate).await.unwrap().size, 5);
            assert_eq!(std::fs::read(remote.join("final.txt")).unwrap(), b"hello");

            let (sidecar, _) = fs
                .create(media, &nfs_name("._clip.mov"), sattr3::default())
                .await
                .unwrap();
            fs.write(sidecar, 0, b"finder-info").await.unwrap();
            assert_eq!(fs.read(sidecar, 0, 64).await.unwrap().0, b"finder-info");
            assert!(!remote.join("media/._clip.mov").exists());
            fs.remove(media, &nfs_name("._clip.mov")).await.unwrap();
            assert!(fs.lookup(media, &nfs_name("._clip.mov")).await.is_err());

            let (link, _) = fs
                .symlink(
                    root,
                    &nfs_name("latest"),
                    &nfs_name("final.txt"),
                    &sattr3::default(),
                )
                .await
                .unwrap();
            assert_eq!(fs.readlink(link).await.unwrap().0, b"final.txt");

            for index in 0..25 {
                fs.create(
                    media,
                    &nfs_name(&format!("shot-{index:02}.jpg")),
                    sattr3::default(),
                )
                .await
                .unwrap();
            }
            let mut seen = Vec::new();
            let mut cookie = 0;
            loop {
                let page = fs.readdir(media, cookie, 7).await.unwrap();
                for entry in &page.entries {
                    seen.push(String::from_utf8(entry.name.0.clone()).unwrap());
                    cookie = entry.fileid;
                }
                if page.end {
                    break;
                }
            }
            assert_eq!(seen.len(), 26);
            assert!(seen.contains(&"clip.mov".to_string()));

            assert!(matches!(
                fs.remove(root, &nfs_name("media")).await,
                Err(nfsstat3::NFS3ERR_NOTEMPTY)
            ));
            let (dir, _) = fs.mkdir(root, &nfs_name("empty")).await.unwrap();
            assert!(fs.getattr(dir).await.is_ok());
            fs.remove(root, &nfs_name("empty")).await.unwrap();
            assert!(!remote.join("empty").exists());
        });
        vfs.shutdown();
    }
}
