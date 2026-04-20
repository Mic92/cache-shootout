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
pub enum S3Backend {
    Minio,
    RustFs,
}

#[derive(Clone, Copy)]
pub enum Server {
    /// `Zstd` exercises harmonia's actix `Compress` middleware: same NAR
    /// stream, transfer-encoded on the fly when the client asks for it.
    Harmonia(Compression),
    NixServe,
    NixServeNg,
    /// nix-serve-ng behind an nginx reverse proxy that zstd-encodes the
    /// response on the fly. Models the common "dumb cache + compressing edge"
    /// deployment without touching the cache server itself.
    NixServeNgNginxZstd,
    /// ncps proxying a local upstream, benchmarked warm so we measure ncps'
    /// own serving path. `Zstd` uses a zstd flat-file upstream so ncps caches
    /// and re-serves `.nar.zst` instead of raw `.nar`.
    Ncps(Compression),
    /// nginx serving a `nix copy --to file://...` flat-file cache. This is the
    /// "just serve bytes off disk" baseline the dynamic servers are up against.
    Nginx(Compression),
    /// atticd with a sqlite DB and local chunk storage. The closure is pushed
    /// up-front via `attic push`, so the benchmark measures pull throughput.
    Attic(Compression),
    /// An S3-compatible object store holding a `nix copy --to s3://` flat-file
    /// cache. The bucket is made anonymously readable so the bench client
    /// fetches `http://host:port/<bucket>/<key>` directly without sigv4;
    /// measures the object store's plain GET path, not any Nix-aware code.
    S3(S3Backend, Compression),
    /// snix-store daemon (object-store + redb on disk) fronted by nar-bridge
    /// for the HTTP binary cache protocol. Closure is pushed up-front via
    /// `snix-store copy`; nar-bridge re-assembles the NAR per request and, if
    /// asked, zstd-encodes it via HTTP Content-Encoding.
    Snix(Compression),
}

impl Server {
    pub fn all() -> &'static [Server] {
        &[
            Server::Harmonia(Compression::None),
            Server::Harmonia(Compression::Zstd),
            Server::NixServe,
            Server::NixServeNg,
            Server::NixServeNgNginxZstd,
            Server::Ncps(Compression::None),
            Server::Ncps(Compression::Zstd),
            Server::Nginx(Compression::None),
            Server::Nginx(Compression::Zstd),
            Server::Attic(Compression::None),
            Server::Attic(Compression::Zstd),
            Server::S3(S3Backend::Minio, Compression::None),
            Server::S3(S3Backend::Minio, Compression::Zstd),
            Server::S3(S3Backend::RustFs, Compression::None),
            Server::S3(S3Backend::RustFs, Compression::Zstd),
            Server::Snix(Compression::None),
            Server::Snix(Compression::Zstd),
        ]
    }

    pub fn name(self) -> &'static str {
        match self {
            Server::Harmonia(Compression::None) => "harmonia-none",
            Server::Harmonia(Compression::Zstd) => "harmonia-zstd",
            Server::NixServe => "nix-serve",
            Server::NixServeNg => "nix-serve-ng",
            Server::NixServeNgNginxZstd => "nix-serve-ng+nginx-zstd",
            Server::Ncps(Compression::None) => "ncps-none",
            Server::Ncps(Compression::Zstd) => "ncps-zstd",
            Server::Nginx(Compression::None) => "nginx-none",
            Server::Nginx(Compression::Zstd) => "nginx-zstd",
            Server::Attic(Compression::None) => "attic-none",
            Server::Attic(Compression::Zstd) => "attic-zstd",
            Server::S3(S3Backend::Minio, Compression::None) => "minio-none",
            Server::S3(S3Backend::Minio, Compression::Zstd) => "minio-zstd",
            Server::S3(S3Backend::RustFs, Compression::None) => "rustfs-none",
            Server::S3(S3Backend::RustFs, Compression::Zstd) => "rustfs-zstd",
            Server::Snix(Compression::None) => "snix-none",
            Server::Snix(Compression::Zstd) => "snix-zstd",
        }
    }

    pub async fn start(self, client: &reqwest::Client, closure_root: &str) -> RunningCache {
        match self {
            Server::Harmonia(c) => start_harmonia(client, c).await,
            Server::NixServe => start_nix_serve(client).await,
            Server::NixServeNg => start_nix_serve_ng(client).await,
            Server::NixServeNgNginxZstd => start_nix_serve_ng_nginx_zstd(client).await,
            Server::Ncps(c) => start_ncps(client, closure_root, c).await,
            Server::Nginx(c) => start_nginx(client, closure_root, c).await,
            Server::Attic(c) => start_attic(client, closure_root, c).await,
            Server::S3(b, c) => start_s3(client, closure_root, b, c).await,
            Server::Snix(c) => start_snix(client, closure_root, c).await,
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

async fn start_harmonia(client: &reqwest::Client, comp: Compression) -> RunningCache {
    let port = pick_unused_port();
    let mut cfg = NamedTempFile::new().unwrap();
    // PR #984 replaced actix `Compress` with a tuned zstd middleware whose
    // parameters live under `[zstd]`; older harmonia ignores unknown keys, so
    // this config works against both before and after the bump.
    write!(
        cfg,
        "bind = \"127.0.0.1:{port}\"\n\
         priority = 30\n\
         enable_compression = true\n\
         [zstd]\n\
         level = 1\n\
         long_distance_matching = true\n\
         window_log = 25\n"
    )
    .unwrap();
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
        name: Server::Harmonia(comp).name(),
        base_url: base,
        // harmonia compresses via HTTP content-encoding negotiation, not in
        // the narinfo URL; the only difference between variants is what the
        // client asks for.
        accept_encoding: match comp {
            Compression::None => None,
            Compression::Zstd => Some("zstd"),
        },
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

async fn start_ncps(
    client: &reqwest::Client,
    closure_root: &str,
    comp: Compression,
) -> RunningCache {
    // ncps is a pull-through proxy and stores whatever bytes the upstream
    // hands it. For the uncompressed variant harmonia is the cheapest local
    // store server; for zstd we point it at a pre-compressed flat-file cache
    // so ncps persists and re-serves `.nar.zst`.
    let upstream = match comp {
        Compression::None => start_harmonia(client, Compression::None).await,
        Compression::Zstd => start_nginx(client, closure_root, Compression::Zstd).await,
    };

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
        name: Server::Ncps(comp).name(),
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
    // The bench closure is fetched from cache.nixos.org, whose key is the
    // cache's default upstream filter; without this flag the entire push
    // would be skipped and every narinfo would 404.
    c.args([
        "push",
        "--ignore-upstream-cache-filter",
        "bench:bench",
        closure_root,
    ]);
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

async fn start_s3(
    client: &reqwest::Client,
    closure_root: &str,
    backend: S3Backend,
    comp: Compression,
) -> RunningCache {
    let dir = TempDir::new().unwrap();
    let data = dir.path().join("data");
    let mc_dir = dir.path().join("mc");
    // nix's narinfo disk cache keys S3 binary caches by their canonical URL
    // (`s3://<bucket>`, params stripped), so two instances with the same
    // bucket name would share cache rows and the second `nix copy` would see
    // "0 paths to copy". Point XDG_CACHE_HOME/HOME at the tempdir so each
    // instance starts from a clean slate (and ignores ~/.aws).
    let nix_home = dir.path().join("nix-home");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&mc_dir).unwrap();
    std::fs::create_dir_all(&nix_home).unwrap();

    let port = pick_unused_port();
    let user = "benchadmin";
    let pass = "benchadmin12345";

    let mut cmd;
    match backend {
        S3Backend::Minio => {
            cmd = Command::new(bin("MINIO_BIN", "minio"));
            cmd.args([
                "server",
                data.to_str().unwrap(),
                "--quiet",
                "--address",
                &format!("127.0.0.1:{port}"),
                "--console-address",
                // minio insists on binding a console; park it on an ephemeral
                // port we never touch.
                &format!("127.0.0.1:{}", pick_unused_port()),
            ])
            .env("MINIO_ROOT_USER", user)
            .env("MINIO_ROOT_PASSWORD", pass)
            .env("MINIO_BROWSER", "off")
            .env("HOME", dir.path());
        }
        S3Backend::RustFs => {
            cmd = Command::new(bin("RUSTFS_BIN", "rustfs"));
            cmd.args([
                "server",
                data.to_str().unwrap(),
                "--address",
                &format!("127.0.0.1:{port}"),
                "--access-key",
                user,
                "--secret-key",
                pass,
            ])
            .env("RUSTFS_CONSOLE_ENABLE", "false")
            // Background scanner / heal threads add noise to a single-node
            // throughput benchmark.
            .env("RUSTFS_SCANNER_ENABLED", "false")
            .env("RUSTFS_HEAL_ENABLED", "false")
            .env("RUST_LOG", "warn")
            .env("HOME", dir.path());
        }
    }
    cmd.stderr(Stdio::null());
    let mut guard = spawn(cmd, "s3 backend");
    guard.keep(dir);
    let pid = guard_pid(&guard);
    // S3 backends accept TCP before the storage layer is ready; mc retries
    // ListBuckets internally so the alias/mb steps below double as readiness.
    wait_for_port(port, pid, Duration::from_secs(60)).await;

    let endpoint = format!("http://127.0.0.1:{port}");
    let bucket = "nix-cache";

    // Use minio's `mc` against either backend (both speak S3) to create the
    // bucket and open it for anonymous GETs, so the bench client can fetch
    // `http://endpoint/<bucket>/<key>` without signing requests.
    let mc = bin("MC_BIN", "mc");
    let mc_run = |args: &[&str], what: &str| {
        let mut c = Command::new(&mc);
        c.arg("--config-dir").arg(&mc_dir).args(args);
        run(&mut c, what);
    };
    // rustfs (and minio on slow disks) accepts TCP before the storage layer
    // is ready and answers `Service not ready`; mc does not retry that, so
    // poll `alias set` ourselves until it sticks.
    let alias_ok = (0..60).any(|_| {
        let ok = Command::new(&mc)
            .arg("--config-dir")
            .arg(&mc_dir)
            .args(["alias", "set", "bench", &endpoint, user, pass])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            std::thread::sleep(Duration::from_millis(500));
        }
        ok
    });
    assert!(alias_ok, "s3 backend at {endpoint} never became ready");
    mc_run(&["mb", &format!("bench/{bucket}")], "mc mb");
    mc_run(
        &["anonymous", "set", "download", &format!("bench/{bucket}")],
        "mc anonymous set download",
    );

    let store_url = format!(
        "s3://{bucket}?endpoint={endpoint}&region=us-east-1&compression={}",
        comp.nix_param()
    );
    eprintln!("s3: nix copy closure to {store_url} ...");
    run(
        Command::new("nix")
            .args([
                "--extra-experimental-features",
                "nix-command",
                "copy",
                "--to",
                &store_url,
                closure_root,
            ])
            .env("AWS_ACCESS_KEY_ID", user)
            .env("AWS_SECRET_ACCESS_KEY", pass)
            .env("HOME", &nix_home)
            .env("XDG_CACHE_HOME", &nix_home)
            .env_remove("NIX_REMOTE"),
        "nix copy (s3)",
    );

    let base = format!("{endpoint}/{bucket}");
    wait_for_cache_info(client, &base, Duration::from_secs(30)).await;

    RunningCache {
        name: Server::S3(backend, comp).name(),
        base_url: base,
        accept_encoding: None,
        _guards: vec![guard],
    }
}

async fn start_snix(
    client: &reqwest::Client,
    closure_root: &str,
    comp: Compression,
) -> RunningCache {
    let dir = TempDir::new().unwrap();
    let data = dir.path().to_path_buf();

    // Both `snix-store copy` and nar-bridge can open the on-disk blob /
    // directory / pathinfo backends directly. We skip the gRPC daemon hop:
    // co-locating nar-bridge with the storage is the realistic deployment for
    // a binary cache, and routing every chunk through tonic added ~40 ms per
    // call here, dominating the benchmark.
    let blob = format!("objectstore+file:{}/blobs", data.display());
    let dirs = format!("redb:{}/directories.redb", data.display());
    let pinfo = format!("redb:{}/pathinfo.redb", data.display());
    let snix_env = |c: &mut Command| {
        c.env("BLOB_SERVICE_ADDR", &blob)
            .env("DIRECTORY_SERVICE_ADDR", &dirs)
            .env("PATH_INFO_SERVICE_ADDR", &pinfo)
            // The tonic trace propagator logs a WARN per request when no OTEL
            // layer is configured; silence it so it doesn't swamp bench output.
            .env("RUST_LOG", "error");
    };

    // `snix-store copy` ingests from /nix/store using the path metadata list
    // emitted by `nix path-info --json`. Nix ≥2.23 returns an object keyed by
    // store path; snix wants a flat list with a `path` field, so reshape it.
    eprintln!("snix: copying closure {closure_root} ...");
    let pi = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "path-info",
            "--json",
            "--closure-size",
            "--recursive",
            closure_root,
        ])
        .output()
        .expect("nix path-info --json");
    assert!(
        pi.status.success(),
        "nix path-info --json failed: {}",
        String::from_utf8_lossy(&pi.stderr)
    );
    let mut jq = Command::new("jq")
        .arg(
            "if type == \"array\" then . \
             else to_entries | map({path: .key} + .value) end",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("jq");
    jq.stdin.take().unwrap().write_all(&pi.stdout).unwrap();
    let jq_out = jq.wait_with_output().expect("jq wait");
    assert!(jq_out.status.success(), "jq reshape failed");

    let mut copy = Command::new(bin("SNIX_STORE_BIN", "snix-store"));
    copy.args(["copy", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    snix_env(&mut copy);
    let mut copy_child = copy.spawn().expect("snix-store copy");
    copy_child
        .stdin
        .take()
        .unwrap()
        .write_all(&jq_out.stdout)
        .unwrap();
    let copy_status = copy_child.wait().expect("snix-store copy wait");
    assert!(copy_status.success(), "snix-store copy failed");

    // nar-bridge speaks the Nix HTTP binary cache protocol on top of those
    // backends and zstd-encodes the NAR stream via HTTP Content-Encoding when
    // the client asks for it.
    let port = pick_unused_port();
    let mut cmd = Command::new(bin("NAR_BRIDGE_BIN", "snix-nar-bridge"));
    cmd.args(["-l", &format!("127.0.0.1:{port}")]);
    snix_env(&mut cmd);
    cmd.stderr(Stdio::null());
    let mut bridge_guard = spawn(cmd, "snix-nar-bridge");
    bridge_guard.keep(dir);
    let bridge_pid = guard_pid(&bridge_guard);
    wait_for_port(port, bridge_pid, Duration::from_secs(30)).await;
    let base = format!("http://127.0.0.1:{port}");
    wait_for_cache_info(client, &base, Duration::from_secs(30)).await;

    RunningCache {
        name: Server::Snix(comp).name(),
        base_url: base,
        accept_encoding: match comp {
            Compression::None => None,
            Compression::Zstd => Some("zstd"),
        },
        _guards: vec![bridge_guard],
    }
}

fn guard_pid(g: &ProcessGuard) -> u32 {
    // Safe: child is always Some until Drop.
    g.child.as_ref().unwrap().id()
}
