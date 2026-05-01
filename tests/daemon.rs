use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
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

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn start_daemon(allowed: &Path) -> Daemon {
    let port = free_port();
    let child = Command::new(bin())
        .arg("daemon")
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--token")
        .arg(TOKEN)
        .arg("--allow-root")
        .arg(allowed)
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

    let output = mobfs(&local, &["run", "cat", "c.txt"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("new remote"));
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
}
