use super::{bin, pick_unused_port, run, wait_for_cache_info, wait_for_port, ProcessGuard};
use base64::Engine;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;
use tempfile::{NamedTempFile, TempDir};

/// A running binary cache reachable at `base_url` (no trailing slash).
pub struct RunningCache {
    pub name: &'static str,
    pub base_url: String,
    /// `Accept-Encoding` to send for this server, when the benchmark wants the
    /// server (or a fronting proxy) to apply HTTP transfer compression. The
    /// drained byte count then reflects the compressed wire size.
    pub accept_encoding: Option<&'static str>,
    pub _guards: Vec<ProcessGuard>,
}

#[derive(Clone, Copy)]
pub enum Compression {
    None,
    Zstd,
}

impl Compression {
    fn nix_param(self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Zstd => "zstd",
        }
    }
}

#[derive(Clone, Copy)]
pub enum Server {
    Harmonia,
    NixServe,
    NixServeNg,
    /// nix-serve-ng behind an nginx reverse proxy that zstd-encodes the
    /// response on the fly. Models the common "dumb cache + compressing edge"
    /// deployment without touching the cache server itself.
    NixServeNgNginxZstd,
    /// ncps proxying a local harmonia upstream; benchmarked after a warm-up
    /// pass so we measure ncps' own serving path, not the upstream fetch.
    Ncps,
    /// nginx serving a `nix copy --to file://...` flat-file cache. This is the
    /// "just serve bytes off disk" baseline the dynamic servers are up against.
    Nginx(Compression),
    /// atticd with a sqlite DB and local chunk storage. The closure is pushed
    /// up-front via `attic push`, so the benchmark measures pull throughput.
    Attic(Compression),
}

impl Server {
    pub fn all() -> &'static [Server] {
        // Dynamic servers stream the store and have no NAR-level compression
        // knob, so they appear once. nginx/attic persist NARs on disk and are
        // run with both `none` and `zstd` so the wire-size trade-off shows up.
        &[
            Server::Harmonia,
            Server::NixServe,
            Server::NixServeNg,
            Server::NixServeNgNginxZstd,
            Server::Ncps,
            Server::Nginx(Compression::None),
            Server::Nginx(Compression::Zstd),
            Server::Attic(Compression::None),
            Server::Attic(Compression::Zstd),
        ]
    }

    pub fn name(self) -> &'static str {
        match self {
            Server::Harmonia => "harmonia",
            Server::NixServe => "nix-serve",
            Server::NixServeNg => "nix-serve-ng",
            Server::NixServeNgNginxZstd => "nix-serve-ng+nginx-zstd",
            Server::Ncps => "ncps",
            Server::Nginx(Compression::None) => "nginx-none",
            Server::Nginx(Compression::Zstd) => "nginx-zstd",
            Server::Attic(Compression::None) => "attic-none",
            Server::Attic(Compression::Zstd) => "attic-zstd",
        }
    }

    pub async fn start(self, client: &reqwest::Client, closure_root: &str) -> RunningCache {
        match self {
            Server::Harmonia => start_harmonia(client).await,
            Server::NixServe => start_nix_serve(client).await,
            Server::NixServeNg => start_nix_serve_ng(client).await,
            Server::NixServeNgNginxZstd => start_nix_serve_ng_nginx_zstd(client).await,
            Server::Ncps => start_ncps(client).await,
            Server::Nginx(c) => start_nginx(client, closure_root, c).await,
            Server::Attic(c) => start_attic(client, closure_root, c).await,
        }
    }
}

fn spawn(mut cmd: Command, what: &str) -> ProcessGuard {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Keep stderr on the parent so server crashes are visible in bench output.
        .spawn()
        .unwrap_or_else(|e| panic!("{what}: spawn failed: {e}"));
    ProcessGuard::new(child)
}

async fn start_harmonia(client: &reqwest::Client) -> RunningCache {
    let port = pick_unused_port();
    let mut cfg = NamedTempFile::new().unwrap();
    writeln!(cfg, "bind = \"127.0.0.1:{port}\"\npriority = 30").unwrap();
    cfg.flush().unwrap();

    let mut cmd = Command::new(bin("HARMONIA_BIN", "harmonia-cache"));
    cmd.env("CONFIG_FILE", cfg.path()).env("RUST_LOG", "warn");
    let mut guard = spawn(cmd, "harmonia-cache");
    guard.keep(cfg);

    let pid = guard_pid(&guard);
    wait_for_port(port, pid, Duration::from_secs(30)).await;
    let base = format!("http://127.0.0.1:{port}");
    wait_for_cache_info(client, &base, Duration::from_secs(30)).await;

    RunningCache {
        name: "harmonia",
        base_url: base,
        accept_encoding: None,
        _guards: vec![guard],
    }
}

async fn start_nix_serve(client: &reqwest::Client) -> RunningCache {
    let port = pick_unused_port();
    // nix-serve wraps starman; --listen / --workers are starman flags.
    let mut cmd = Command::new(bin("NIX_SERVE_BIN", "nix-serve"));
    cmd.args(["--listen", &format!("127.0.0.1:{port}"), "--workers", "8"]);
    cmd.stderr(Stdio::null()); // starman access log is noisy
    let guard = spawn(cmd, "nix-serve");

    let pid = guard_pid(&guard);
    wait_for_port(port, pid, Duration::from_secs(60)).await;
    let base = format!("http://127.0.0.1:{port}");
    wait_for_cache_info(client, &base, Duration::from_secs(30)).await;

    RunningCache {
        name: "nix-serve",
        base_url: base,
        accept_encoding: None,
        _guards: vec![guard],
    }
}

async fn start_nix_serve_ng(client: &reqwest::Client) -> RunningCache {
    let port = pick_unused_port();
    let mut cmd = Command::new(bin("NIX_SERVE_NG_BIN", "nix-serve"));
    cmd.args([
        "--host",
        "127.0.0.1",
        "--port",
        &port.to_string(),
        "--quiet",
    ]);
    let guard = spawn(cmd, "nix-serve-ng");

    let pid = guard_pid(&guard);
    wait_for_port(port, pid, Duration::from_secs(30)).await;
    let base = format!("http://127.0.0.1:{port}");
    wait_for_cache_info(client, &base, Duration::from_secs(30)).await;

    RunningCache {
        name: "nix-serve-ng",
        base_url: base,
        accept_encoding: None,
        _guards: vec![guard],
    }
}

async fn start_nix_serve_ng_nginx_zstd(client: &reqwest::Client) -> RunningCache {
    let upstream = start_nix_serve_ng(client).await;

    let dir = TempDir::new().unwrap();
    let port = pick_unused_port();
    let cfg = dir.path().join("nginx.conf");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        &cfg,
        format!(
            r#"daemon off;
worker_processes auto;
pid {pid};
error_log stderr warn;
events {{ worker_connections 1024; }}
http {{
  access_log off;
  client_body_temp_path {tmp}/body;
  proxy_temp_path {tmp}/proxy;
  fastcgi_temp_path {tmp}/fcgi;
  uwsgi_temp_path {tmp}/uwsgi;
  scgi_temp_path {tmp}/scgi;

  zstd on;
  zstd_comp_level 3;
  zstd_min_length 256;
  zstd_types *;

  server {{
    listen 127.0.0.1:{port};
    location / {{
      proxy_pass {upstream};
      proxy_http_version 1.1;
      proxy_buffering off;
    }}
  }}
}}
"#,
            pid = dir.path().join("nginx.pid").display(),
            tmp = tmp.display(),
            upstream = upstream.base_url,
        ),
    )
    .unwrap();

    let mut cmd = Command::new(bin("NGINX_BIN", "nginx"));
    cmd.args([
        "-c",
        cfg.to_str().unwrap(),
        "-p",
        dir.path().to_str().unwrap(),
        "-e",
        "stderr",
    ]);
    let mut guard = spawn(cmd, "nginx (zstd proxy)");
    guard.keep(dir);

    let pid = guard_pid(&guard);
    wait_for_port(port, pid, Duration::from_secs(30)).await;
    let base = format!("http://127.0.0.1:{port}");
    wait_for_cache_info(client, &base, Duration::from_secs(30)).await;

    let mut guards = upstream._guards;
    guards.push(guard);
    RunningCache {
        name: "nix-serve-ng+nginx-zstd",
        base_url: base,
        // Ask nginx for zstd; reqwest has no zstd decoder enabled, so the
        // drained byte count is the compressed wire size.
        accept_encoding: Some("zstd"),
        _guards: guards,
    }
}

async fn start_ncps(client: &reqwest::Client) -> RunningCache {
    // ncps is a pull-through proxy, so it needs an upstream. Use harmonia for
    // that role since it serves the local store with no extra setup.
    let upstream = start_harmonia(client).await;

    let port = pick_unused_port();
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("store");
    let db = dir.path().join("db.sqlite");
    std::fs::create_dir_all(&storage).unwrap();
    let db_url = format!("sqlite:{}", db.display());

    // ncps ships its migrations alongside a dbmate wrapper. The wrapper picks
    // the sqlite migrations subdir from DATABASE_URL; NCPS_DB_MIGRATIONS_DIR /
    // NCPS_DB_SCHEMA_DIR (set by the flake devShell) tell it where to look.
    run(
        Command::new(bin("NCPS_DBMATE_BIN", "dbmate-ncps"))
            .env("DATABASE_URL", &db_url)
            .args(["--no-dump-schema", "up"]),
        "ncps dbmate up",
    );

    let mut cmd = Command::new(bin("NCPS_BIN", "ncps"));
    cmd.args([
        "serve",
        "--server-addr",
        &format!("127.0.0.1:{port}"),
        "--cache-hostname",
        "localhost",
        "--cache-storage-local",
        storage.to_str().unwrap(),
        "--cache-database-url",
        &db_url,
        "--cache-upstream-url",
        &upstream.base_url,
    ]);
    cmd.stderr(Stdio::null());
    let mut guard = spawn(cmd, "ncps");
    guard.keep(dir);

    let pid = guard_pid(&guard);
    wait_for_port(port, pid, Duration::from_secs(30)).await;
    let base = format!("http://127.0.0.1:{port}");
    wait_for_cache_info(client, &base, Duration::from_secs(30)).await;

    let mut guards = upstream._guards;
    guards.push(guard);
    RunningCache {
        name: "ncps",
        base_url: base,
        accept_encoding: None,
        _guards: guards,
    }
}

async fn start_nginx(
    client: &reqwest::Client,
    closure_root: &str,
    comp: Compression,
) -> RunningCache {
    let dir = TempDir::new().unwrap();
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&cache).unwrap();

    // Materialise the closure as a flat-file binary cache so nginx only has to
    // ship pre-baked bytes. The `compression` store param controls whether
    // narinfos point at `.nar` or `.nar.zst`.
    let dest = format!(
        "file://{}?compression={}",
        cache.display(),
        comp.nix_param()
    );
    eprintln!("nginx: nix copy closure to {dest} ...");
    run(
        Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command",
                "copy",
                "--to",
                &dest,
                closure_root,
            ])
            .env_remove("NIX_REMOTE"),
        "nix copy (file cache)",
    );

    let port = pick_unused_port();
    let logs = dir.path().join("logs");
    let tmp = dir.path().join("tmp");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    let cfg = dir.path().join("nginx.conf");
    std::fs::write(
        &cfg,
        format!(
            r#"daemon off;
worker_processes auto;
pid {pid};
error_log stderr warn;
events {{ worker_connections 1024; }}
http {{
  access_log off;
  sendfile on;
  tcp_nopush on;
  client_body_temp_path {tmp}/body;
  proxy_temp_path {tmp}/proxy;
  fastcgi_temp_path {tmp}/fcgi;
  uwsgi_temp_path {tmp}/uwsgi;
  scgi_temp_path {tmp}/scgi;
  server {{
    listen 127.0.0.1:{port};
    root {root};
    location / {{ }}
  }}
}}
"#,
            pid = dir.path().join("nginx.pid").display(),
            tmp = tmp.display(),
            root = cache.display(),
        ),
    )
    .unwrap();

    let mut cmd = Command::new(bin("NGINX_BIN", "nginx"));
    cmd.args([
        "-c",
        cfg.to_str().unwrap(),
        "-p",
        dir.path().to_str().unwrap(),
        "-e",
        "stderr",
    ]);
    let mut guard = spawn(cmd, "nginx");
    guard.keep(dir);

    let pid = guard_pid(&guard);
    wait_for_port(port, pid, Duration::from_secs(30)).await;
    let base = format!("http://127.0.0.1:{port}");
    wait_for_cache_info(client, &base, Duration::from_secs(30)).await;

    RunningCache {
        name: Server::Nginx(comp).name(),
        base_url: base,
        accept_encoding: None,
        _guards: vec![guard],
    }
}

async fn start_attic(
    client: &reqwest::Client,
    closure_root: &str,
    comp: Compression,
) -> RunningCache {
    let port = pick_unused_port();
    let dir = TempDir::new().unwrap();
    let storage = dir.path().join("storage");
    let client_home = dir.path().join("client-home");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::create_dir_all(&client_home).unwrap();
    let db = dir.path().join("server.db");

    // 256-bit HS256 secret; atticd derives signing keys from this.
    let secret: [u8; 32] = rand::random();
    let secret_b64 = base64::engine::general_purpose::STANDARD.encode(secret);

    let cfg_path = dir.path().join("server.toml");
    std::fs::write(
        &cfg_path,
        format!(
            r#"listen = "127.0.0.1:{port}"
allowed-hosts = []

[database]
url = "sqlite://{db}?mode=rwc"

[storage]
type = "local"
path = "{storage}"

[chunking]
nar-size-threshold = 65536
min-size = 16384
avg-size = 65536
max-size = 262144

[compression]
type = "{compression}"

[garbage-collection]
interval = "0 hours"
"#,
            db = db.display(),
            storage = storage.display(),
            compression = comp.nix_param(),
        ),
    )
    .unwrap();

    let mut cmd = Command::new(bin("ATTICD_BIN", "atticd"));
    cmd.args(["-f", cfg_path.to_str().unwrap(), "--mode", "monolithic"])
        .env("ATTIC_SERVER_TOKEN_HS256_SECRET_BASE64", &secret_b64)
        .env("RUST_LOG", "warn");
    cmd.stderr(Stdio::null());
    let mut guard = spawn(cmd, "atticd");
    guard.keep(dir);

    let pid = guard_pid(&guard);
    wait_for_port(port, pid, Duration::from_secs(60)).await;

    // Mint an admin token, create a cache and push the closure so the
    // benchmark exercises atticd's read path only.
    let token_out = Command::new(bin("ATTICADM_BIN", "atticadm"))
        .args([
            "-f",
            cfg_path.to_str().unwrap(),
            "make-token",
            "--sub",
            "bench",
            "--validity",
            "1y",
            "--pull",
            "*",
            "--push",
            "*",
            "--create-cache",
            "*",
            "--configure-cache",
            "*",
        ])
        .env("ATTIC_SERVER_TOKEN_HS256_SECRET_BASE64", &secret_b64)
        .output()
        .expect("atticadm make-token");
    assert!(
        token_out.status.success(),
        "atticadm make-token failed: {}",
        String::from_utf8_lossy(&token_out.stderr)
    );
    let token = String::from_utf8(token_out.stdout)
        .unwrap()
        .trim()
        .to_string();

    let endpoint = format!("http://127.0.0.1:{port}/");
    // Isolate the attic client config so we don't clobber the user's.
    let attic_env = |c: &mut Command| {
        c.env("XDG_CONFIG_HOME", &client_home)
            .env("HOME", &client_home);
    };

    let attic = bin("ATTIC_BIN", "attic");
    let mut c = Command::new(&attic);
    c.args(["login", "bench", &endpoint, &token]);
    attic_env(&mut c);
    run(&mut c, "attic login");

    // Caches default to private; create as public so the unauthenticated
    // bench client can pull without a token.
    let mut c = Command::new(&attic);
    c.args(["cache", "create", "bench:bench", "--public"]);
    attic_env(&mut c);
    run(&mut c, "attic cache create");

    eprintln!("attic: pushing closure {closure_root} ...");
    let mut c = Command::new(&attic);
    c.args(["push", "bench:bench", closure_root]);
    attic_env(&mut c);
    run(&mut c, "attic push");

    // Public-facing binary cache for cache `bench` lives under /bench/.
    let base = format!("http://127.0.0.1:{port}/bench");
    wait_for_cache_info(client, &base, Duration::from_secs(30)).await;

    RunningCache {
        name: Server::Attic(comp).name(),
        base_url: base,
        accept_encoding: None,
        _guards: vec![guard],
    }
}

fn guard_pid(g: &ProcessGuard) -> u32 {
    // Safe: child is always Some until Drop.
    g.child.as_ref().unwrap().id()
}
