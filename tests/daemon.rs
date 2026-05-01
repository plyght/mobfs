use std::fs;
use std::net::TcpListener;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const TOKEN: &str = "test-token";

struct Daemon {
    child: Child,
    port: u16,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_mobfs")
}

fn port_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_daemon(allowed: &Path) -> Daemon {
    start_daemon_with_args(&["--allow-root", allowed.to_str().unwrap()])
}

fn start_daemon_on_port(allowed: &Path, port: u16) -> Daemon {
    start_daemon_with_args_on_port(&["--allow-root", allowed.to_str().unwrap()], port)
}

fn start_daemon_with_args(args: &[&str]) -> Daemon {
    let _guard = port_lock().lock().unwrap();
    let port = free_port();
    start_daemon_with_args_on_port(args, port)
}

fn start_daemon_with_args_on_port(args: &[&str], port: u16) -> Daemon {
    let child = Command::new(bin())
        .arg("daemon")
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--token")
        .arg(TOKEN)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for_daemon(port);
    Daemon { child, port }
}

fn wait_for_daemon(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("daemon did not start");
}

fn mobfs(cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .current_dir(cwd)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn token_and_setup_are_safe_by_default() {
    let temp = TempDir::new().unwrap();
    let output = mobfs(temp.path(), &["token"]);
    assert!(output.status.success());
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(token.len(), 64);
    assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));

    let remote = temp.path().join("remote");
    let output = mobfs(
        temp.path(),
        &[
            "setup",
            remote.to_str().unwrap(),
            "--host",
            "devbox",
            "--name",
            "app",
            "--token",
            TOKEN,
        ],
    );
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--bind 127.0.0.1:7727"), "{stdout}");
    assert!(stdout.contains("devbox:"), "{stdout}");
    assert!(stdout.contains("--ssh-tunnel --name app"), "{stdout}");
}

#[test]
fn daemon_token_error_is_clear() {
    let temp = TempDir::new().unwrap();
    let allowed = temp.path().join("allowed");
    fs::create_dir_all(&allowed).unwrap();
    let output = Command::new(bin())
        .arg("daemon")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--allow-root")
        .arg(&allowed)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("daemon token missing"), "{stderr}");
}

#[test]
fn daemon_mount_push_pull_and_run_roundtrip() {
    let temp = TempDir::new().unwrap();
    let remote = temp.path().join("remote");
    let local = temp.path().join("local");
    fs::create_dir_all(&remote).unwrap();
    fs::write(remote.join("a.txt"), "remote").unwrap();
    let daemon = start_daemon(&remote);
    let remote_arg = format!("127.0.0.1:{}", remote.display());

    let output = mobfs(
        temp.path(),
        &[
            "mount",
            &remote_arg,
            "--local",
            local.to_str().unwrap(),
            "--token",
            TOKEN,
            "--port",
            &daemon.port.to_string(),
            "--no-open",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(local.join("a.txt")).unwrap(), "remote");
    assert!(
        !fs::read_to_string(local.join(".mobfs.toml"))
            .unwrap()
            .contains(TOKEN)
    );
    assert_eq!(
        fs::read_to_string(local.join(".mobfs").join("token"))
            .unwrap()
            .trim(),
        TOKEN
    );

    let large = vec![b'x'; 2 * 1024 * 1024 + 17];
    fs::write(local.join("big.bin"), &large).unwrap();
    let output = mobfs(&local, &["push"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(remote.join("big.bin")).unwrap(), large);

    fs::write(remote.join("c.txt"), "new remote").unwrap();
    let output = mobfs(&local, &["pull"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(local.join("c.txt")).unwrap(),
        "new remote"
    );

    fs::write(local.join("local-run.txt"), "synced before run").unwrap();
    let output = mobfs(&local, &["run", "cat", "local-run.txt"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("synced before run"));
    assert_eq!(
        fs::read_to_string(remote.join("local-run.txt")).unwrap(),
        "synced before run"
    );

    fs::write(local.join(".DS_Store"), "mac noise").unwrap();
    let output = mobfs(&local, &["push"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!remote.join(".DS_Store").exists());

    fs::write(local.join("dry-run.txt"), "not pushed").unwrap();
    let output = mobfs(&local, &["push", "--dry-run"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!remote.join("dry-run.txt").exists());
}

#[test]
fn daemon_requires_explicit_root_policy() {
    let output = Command::new(bin())
        .arg("daemon")
        .arg("--bind")
        .arg("127.0.0.1:0")
        .arg("--token")
        .arg(TOKEN)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("requires --allow-root"), "{stderr}");
}

#[test]
fn daemon_allows_descendants_of_allowed_roots() {
    let temp = TempDir::new().unwrap();
    let parent = temp.path().join("parent");
    let remote = parent.join("project");
    let local = temp.path().join("local");
    fs::create_dir_all(&remote).unwrap();
    fs::write(remote.join("a.txt"), "remote").unwrap();
    let daemon = start_daemon(&parent);
    let remote_arg = format!("127.0.0.1:{}", remote.display());

    let output = mobfs(
        temp.path(),
        &[
            "mount",
            &remote_arg,
            "--local",
            local.to_str().unwrap(),
            "--token",
            TOKEN,
            "--port",
            &daemon.port.to_string(),
            "--no-open",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(local.join("a.txt")).unwrap(), "remote");
}

#[test]
fn daemon_rejects_roots_outside_allowlist() {
    let temp = TempDir::new().unwrap();
    let allowed = temp.path().join("allowed");
    let denied = temp.path().join("denied");
    let local = temp.path().join("local");
    fs::create_dir_all(&allowed).unwrap();
    fs::create_dir_all(&denied).unwrap();
    let daemon = start_daemon(&allowed);
    let remote_arg = format!("127.0.0.1:{}", denied.display());

    let output = mobfs(
        temp.path(),
        &[
            "mount",
            &remote_arg,
            "--local",
            local.to_str().unwrap(),
            "--token",
            TOKEN,
            "--port",
            &daemon.port.to_string(),
            "--no-open",
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not allowed"), "{stderr}");
}

#[test]
fn git_command_runs_on_remote_after_syncing() {
    let temp = TempDir::new().unwrap();
    let remote = temp.path().join("remote");
    let local = temp.path().join("local");
    fs::create_dir_all(&remote).unwrap();
    let daemon = start_daemon(&remote);
    let remote_arg = format!("127.0.0.1:{}", remote.display());

    let output = mobfs(
        temp.path(),
        &[
            "mount",
            &remote_arg,
            "--local",
            local.to_str().unwrap(),
            "--token",
            TOKEN,
            "--port",
            &daemon.port.to_string(),
            "--no-open",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = mobfs(&local, &["git", "init"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    fs::write(local.join("tracked.txt"), "git sees this").unwrap();
    let output = mobfs(&local, &["git", "status", "--short"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("tracked.txt"));
}

#[cfg(unix)]
#[test]
fn sync_preserves_symlinks_and_executable_bits() {
    let temp = TempDir::new().unwrap();
    let remote = temp.path().join("remote");
    let local = temp.path().join("local");
    fs::create_dir_all(&remote).unwrap();
    fs::write(remote.join("tool.sh"), "#!/bin/sh\necho ok\n").unwrap();
    fs::set_permissions(remote.join("tool.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    symlink("tool.sh", remote.join("tool-link")).unwrap();
    let daemon = start_daemon(&remote);
    let remote_arg = format!("127.0.0.1:{}", remote.display());

    let output = mobfs(
        temp.path(),
        &[
            "mount",
            &remote_arg,
            "--local",
            local.to_str().unwrap(),
            "--token",
            TOKEN,
            "--port",
            &daemon.port.to_string(),
            "--no-open",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_link(local.join("tool-link")).unwrap(),
        Path::new("tool.sh")
    );
    assert_eq!(
        fs::metadata(local.join("tool.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );

    fs::write(local.join("local-tool.sh"), "#!/bin/sh\necho local\n").unwrap();
    fs::set_permissions(
        local.join("local-tool.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("local-tool.sh", local.join("local-tool-link")).unwrap();
    let output = mobfs(&local, &["push"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_link(remote.join("local-tool-link")).unwrap(),
        Path::new("local-tool.sh")
    );
    assert_eq!(
        fs::metadata(remote.join("local-tool.sh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111,
        0o111
    );
}

#[test]
fn mountfs_smoke_test_when_enabled() {
    if std::env::var("MOBFS_RUN_FUSE_TESTS").ok().as_deref() != Some("1") {
        return;
    }
    let temp = TempDir::new().unwrap();
    let remote = temp.path().join("remote");
    let mountpoint = temp.path().join("mnt");
    fs::create_dir_all(&remote).unwrap();
    fs::write(remote.join("a.txt"), "remote").unwrap();
    let daemon = start_daemon(&remote);
    let remote_arg = format!("127.0.0.1:{}", remote.display());
    let mut child = Command::new(bin())
        .arg("mountfs")
        .arg(&remote_arg)
        .arg(&mountpoint)
        .arg("--token")
        .arg(TOKEN)
        .arg("--port")
        .arg(daemon.port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !mountpoint.join("a.txt").exists() {
        thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        fs::read_to_string(mountpoint.join("a.txt")).unwrap(),
        "remote"
    );
    fs::write(mountpoint.join("b.txt"), "local through fuse").unwrap();
    assert_eq!(
        fs::read_to_string(remote.join("b.txt")).unwrap(),
        "local through fuse"
    );
    fs::create_dir_all(mountpoint.join("dir")).unwrap();
    fs::write(mountpoint.join("dir").join("agent.txt"), "agent write").unwrap();
    fs::rename(
        mountpoint.join("dir").join("agent.txt"),
        mountpoint.join("dir").join("agent-renamed.txt"),
    )
    .unwrap();
    let large = vec![b'z'; 1024 * 1024 + 3];
    fs::write(mountpoint.join("large.bin"), &large).unwrap();
    assert_eq!(
        fs::read_to_string(remote.join("dir").join("agent-renamed.txt")).unwrap(),
        "agent write"
    );
    assert_eq!(fs::read(remote.join("large.bin")).unwrap(), large);
    let output = Command::new("git")
        .arg("init")
        .current_dir(&mountpoint)
        .output()
        .unwrap();
    assert!(output.status.success());
    let output = Command::new("git")
        .arg("status")
        .arg("--short")
        .current_dir(&mountpoint)
        .output()
        .unwrap();
    assert!(output.status.success());
    fs::remove_file(mountpoint.join("large.bin")).unwrap();
    assert!(!remote.join("large.bin").exists());
    let _ = if cfg!(target_os = "macos") {
        Command::new("diskutil")
            .arg("unmount")
            .arg(&mountpoint)
            .status()
    } else {
        Command::new("fusermount")
            .arg("-u")
            .arg(&mountpoint)
            .status()
    };
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn sync_stops_on_same_path_conflict() {
    let temp = TempDir::new().unwrap();
    let remote = temp.path().join("remote");
    let local = temp.path().join("local");
    fs::create_dir_all(&remote).unwrap();
    fs::write(remote.join("a.txt"), "base").unwrap();
    let daemon = start_daemon(&remote);
    let remote_arg = format!("127.0.0.1:{}", remote.display());

    let output = mobfs(
        temp.path(),
        &[
            "mount",
            &remote_arg,
            "--local",
            local.to_str().unwrap(),
            "--token",
            TOKEN,
            "--port",
            &daemon.port.to_string(),
            "--no-open",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    fs::write(local.join("a.txt"), "local").unwrap();
    fs::write(remote.join("a.txt"), "remote").unwrap();
    let output = mobfs(&local, &["sync"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("both sides changed"), "{stderr}");
    assert_eq!(
        fs::read_to_string(local.join("a.txt.mobfs-conflict-local")).unwrap(),
        "local"
    );
    assert_eq!(
        fs::read_to_string(local.join("a.txt.mobfs-conflict-remote")).unwrap(),
        "remote"
    );
}

#[test]
fn reconnects_after_daemon_restart_for_later_operations() {
    let temp = TempDir::new().unwrap();
    let remote = temp.path().join("remote");
    let local = temp.path().join("local");
    fs::create_dir_all(&remote).unwrap();
    let _guard = port_lock().lock().unwrap();
    let port = free_port();
    let mut daemon = start_daemon_on_port(&remote, port);
    let remote_arg = format!("127.0.0.1:{}", remote.display());

    let output = mobfs(
        temp.path(),
        &[
            "mount",
            &remote_arg,
            "--local",
            local.to_str().unwrap(),
            "--token",
            TOKEN,
            "--port",
            &daemon.port.to_string(),
            "--no-open",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    daemon = start_daemon_on_port(&remote, port);
    fs::write(local.join("after-restart.txt"), "ok").unwrap();
    let output = mobfs(&local, &["push"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(remote.join("after-restart.txt")).unwrap(),
        "ok"
    );
    drop(daemon);
}
