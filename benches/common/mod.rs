pub mod servers;

use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::time::{sleep, timeout};

/// Kills the spawned server (SIGTERM, then SIGKILL) when the benchmark
/// iteration or the whole run drops it.
pub struct ProcessGuard {
    pub child: Option<Child>,
    // Kept alive so config files / state dirs outlive the server process.
    _resources: Vec<Box<dyn std::any::Any>>,
}

impl ProcessGuard {
    pub fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            _resources: Vec::new(),
        }
    }

    pub fn keep<T: 'static>(&mut self, res: T) {
        self._resources.push(Box::new(res));
    }
}

impl Drop for ProcessGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;

            let pid = Pid::from_raw(child.id() as i32);
            let _ = kill(pid, Signal::SIGTERM);
            for _ in 0..50 {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                    Err(_) => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub fn pick_unused_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind 0")
        .local_addr()
        .expect("local_addr")
        .port()
}

/// Poll the TCP port until it accepts connections, panicking if the child
/// exits early so failures surface with context instead of a generic timeout.
pub async fn wait_for_port(port: u16, pid: u32, timeout_duration: Duration) {
    let res = timeout(timeout_duration, async {
        loop {
            use nix::sys::signal::kill;
            use nix::unistd::Pid;
            if kill(Pid::from_raw(pid as i32), None).is_err() {
                panic!("server pid {pid} died while waiting for port {port}");
            }
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    if res.is_err() {
        panic!("timeout waiting for port {port} after {timeout_duration:?}");
    }
}

/// Wait until `GET {base}/nix-cache-info` returns 2xx. Some servers (starman,
/// atticd) accept TCP before the application is actually routing requests.
pub async fn wait_for_cache_info(client: &reqwest::Client, base: &str, timeout_duration: Duration) {
    let url = format!("{base}/nix-cache-info");
    let res = timeout(timeout_duration, async {
        loop {
            match client.get(&url).send().await {
                Ok(r) if r.status().is_success() => return,
                _ => sleep(Duration::from_millis(200)).await,
            }
        }
    })
    .await;
    if res.is_err() {
        panic!("timeout waiting for {url} after {timeout_duration:?}");
    }
}

pub fn current_system() -> String {
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "config",
            "show",
            "system",
        ])
        .output()
        .expect("nix config show system");
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

pub fn nix_build(flake_ref: &str) -> String {
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "build",
            "--no-link",
            "--print-out-paths",
            flake_ref,
        ])
        .output()
        .expect("nix build");
    assert!(
        out.status.success(),
        "nix build {flake_ref} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Build every workload closure listed in `BENCH_CLOSURES` (comma-separated
/// short names mapping to `.#packages.<system>.closure-<name>`). A literal
/// flake ref (contains `#`) is also accepted for ad-hoc runs.
pub fn build_closures() -> Vec<(String, String)> {
    let system = current_system();
    let spec = std::env::var("BENCH_CLOSURES").unwrap_or_else(|_| "firefox".to_string());
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|name| {
            let flake = if name.contains('#') {
                name.to_string()
            } else {
                format!(".#packages.{system}.closure-{name}")
            };
            eprintln!("building closure '{name}' from {flake} ...");
            let path = nix_build(&flake);
            eprintln!("closure '{name}' built: {path}");
            (name.to_string(), path)
        })
        .collect()
}

pub fn closure_paths(store_path: &str) -> Vec<String> {
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command flakes",
            "path-info",
            "--recursive",
            store_path,
        ])
        .output()
        .expect("nix path-info");
    assert!(
        out.status.success(),
        "nix path-info failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Resolve a server binary: prefer the explicit env var set by the flake
/// devShell (so we benchmark the exact upstream-flake build), otherwise
/// fall back to PATH.
pub fn bin(env: &str, fallback: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| fallback.to_string())
}

/// `/nix/store/abc123-name` -> `abc123`
pub fn store_path_hash(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap()
        .split('-')
        .next()
        .unwrap()
        .to_string()
}

pub fn run(cmd: &mut Command, what: &str) {
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|e| panic!("{what}: spawn failed: {e}"));
    assert!(
        out.status.success(),
        "{what} failed ({}):\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
