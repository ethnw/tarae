//! Step 6e′'s M5 (`target/sb/step6e-design.md`): the M4 tools — curl, cargo, npm, pip, Go, Gradle — through the
//! real path (`sandbox::policy`, `command`, `spawn`, `after_run`), each fetching one package from a registry of the
//! test's own. Where the session takes a run's connections (`Policy.redirected`: TPROXY, tiers A+ and A) the run
//! has no proxy URL, no proxy variable and no proxy line in its Gradle home; elsewhere (tier C) its way is the
//! URL. Nothing leaves the machine: the registries are the test's own servers on this machine's own address, under
//! `example.invalid` names (`TEST_NAMES`) — curl's and cargo's plain HTTP, served here (so they run wherever curl
//! and cargo are: the checks' own image), the others' HTTPS (`Repo.java`, its certificate trusted by each tool's
//! own setting: they need a JDK, and run where the tools are — `cargo xtask linux --image tarae-m5`). A test whose
//! tool isn't here skips itself, saying so. How long each takes is `target/sb/m5/m5.sh`'s to say, not a test's.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use super::packs_tests::{STORE, finish, keytool, on_path, outside_at, serve, skip};

/// The tools fetching over plain HTTP (served here) and over HTTPS (`Repo.java`).
const PLAIN: [&str; 2] = ["curl", "cargo"];
const TLS: [&str; 4] = ["npm", "pip", "go", "gradle"];

/// A tool's registry's name: one each, so each is its own rule and its own line in a report.
fn host(tool: &str) -> String {
    format!("{tool}-m5.example.invalid")
}

/// `contents` at `path`, its folders made.
fn put(path: &Path, contents: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// A setup step outside the sandbox, its failure the test's.
fn setup(c: &mut Command) -> String {
    let out = c.output().unwrap();
    assert!(out.status.success(), "{c:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A zip of `entries` (name, contents) at `out` — Python's `zipfile`, as a wheel's and a Go module's are.
fn zip(python: &Path, out: &Path, entries: &[(&str, &str)]) {
    const ZIP: &str = "import sys, zipfile\nz = zipfile.ZipFile(sys.argv[1], 'w')\na = sys.argv[2:]\n\
                       for n, c in zip(a[::2], a[1::2]):\n    z.writestr(n, c)\nz.close()\n";
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    setup(Command::new(python).arg("-c").arg(ZIP).arg(out).args(entries.iter().flat_map(|(n, c)| [n, c])));
}

/// A gzipped tar at `out` of `top` (a folder under `dir`), as a crate's and an npm package's are.
fn tgz(dir: &Path, top: &str, out: &Path) {
    std::fs::create_dir_all(out.parent().unwrap()).unwrap();
    setup(Command::new("tar").arg("-czf").arg(out).arg("-C").arg(dir).arg(top));
}

/// The key pair for the HTTPS tools' names (`base/server.p12`), its certificate as PEM (`base/repo.pem`) and a
/// trust store of it alone (`base/trust.p12`).
fn certificate(base: &Path, kt: &Path) {
    let p = |f: &str| base.join(f).display().to_string();
    let san = format!("san={}", TLS.map(|t| format!("dns:{}", host(t))).join(","));
    let server = p("server.p12");
    let pair =
        ["-genkeypair", "-alias", "repo", "-keyalg", "EC", "-groupname", "secp256r1", "-validity", "2"];
    let store = ["-storepass", STORE, "-storetype", "PKCS12"];
    let named = ["-dname", "CN=m5.example.invalid", "-ext", &san, "-keystore", &server];
    keytool(kt, &[&pair[..], &named, &store].concat());
    let pem = p("repo.pem");
    keytool(
        kt,
        &["-exportcert", "-rfc", "-alias", "repo", "-keystore", &server, "-file", &pem, store[0], STORE],
    );
    let trust = p("trust.p12");
    keytool(
        kt,
        &[&["-importcert", "-noprompt", "-alias", "repo", "-file", &pem, "-keystore", &trust][..], &store]
            .concat(),
    );
}

/// curl's file and cargo's sparse registry of one crate, in `repo` (plain HTTP).
fn plain_registry(repo: &Path, scratch: &Path) {
    put(&repo.join("curl.txt"), "m5 curl\n");
    let c = scratch.join("crate/mfive-1.0.0");
    put(&c.join("Cargo.toml"), "[package]\nname = \"mfive\"\nversion = \"1.0.0\"\nedition = \"2021\"\n");
    put(&c.join("src/lib.rs"), "pub const M5: u8 = 5;\n");
    let krate = repo.join("cargo/dl/mfive/1.0.0/download");
    tgz(&scratch.join("crate"), "mfive-1.0.0", &krate);
    let sum = crate::sha256::hex(&std::fs::read(&krate).unwrap());
    let dl = format!("http://{}/cargo/dl", host("cargo"));
    put(&repo.join("cargo/config.json"), format!("{{\"dl\":\"{dl}\"}}"));
    let line = format!(
        "{{\"name\":\"mfive\",\"vers\":\"1.0.0\",\"deps\":[],\"cksum\":\"{sum}\",\"features\":{{}},\"yanked\":false}}\n"
    );
    put(&repo.join("cargo/mf/iv/mfive"), line);
}

/// npm's, pip's, Go's and Gradle's packages, in `repo` (HTTPS).
fn tls_registry(repo: &Path, scratch: &Path, python: &Path) {
    // npm: a packument and its tarball
    let n = scratch.join("npm/package");
    put(&n.join("package.json"), "{\"name\":\"m5pad\",\"version\":\"1.0.0\"}\n");
    put(&n.join("index.js"), "module.exports = 5;\n");
    let tarball = repo.join("npm/tarballs/m5pad-1.0.0.tgz");
    tgz(&scratch.join("npm"), "package", &tarball);
    let integrity = super::parse::base64(&crate::sha256::digest(&std::fs::read(&tarball).unwrap()));
    let url = format!("https://{}/npm/tarballs/m5pad-1.0.0.tgz", host("npm"));
    put(
        &repo.join("npm/m5pad"),
        format!(
            "{{\"name\":\"m5pad\",\"dist-tags\":{{\"latest\":\"1.0.0\"}},\"versions\":{{\"1.0.0\":{{\"name\":\"m5pad\",\
             \"version\":\"1.0.0\",\"dist\":{{\"tarball\":\"{url}\",\"integrity\":\"sha256-{integrity}\"}}}}}}}}"
        ),
    );
    // pip: a simple index of one wheel
    let wheel = repo.join("pypi/files/m5six-1.0-py3-none-any.whl");
    let info = "m5six-1.0.dist-info";
    let (metadata, whl, record) =
        (format!("{info}/METADATA"), format!("{info}/WHEEL"), format!("{info}/RECORD"));
    let recorded = format!("m5six.py,,\n{metadata},,\n{whl},,\n{record},,\n");
    zip(
        python,
        &wheel,
        &[
            ("m5six.py", "X = 5\n"),
            (&metadata, "Metadata-Version: 2.1\nName: m5six\nVersion: 1.0\n"),
            (&whl, "Wheel-Version: 1.0\nGenerator: m5\nRoot-Is-Purelib: true\nTag: py3-none-any\n"),
            (&record, &recorded),
        ],
    );
    let sum = crate::sha256::hex(&std::fs::read(&wheel).unwrap());
    put(
        &repo.join("pypi/simple/m5six/index.html"),
        format!(
            "<html><body><a href=\"../../files/m5six-1.0-py3-none-any.whl#sha256={sum}\">\
             m5six-1.0-py3-none-any.whl</a></body></html>\n"
        ),
    );
    // Go: a module proxy of one module
    let v = repo.join("go/m5.example.invalid/hello/@v");
    let module = "module m5.example.invalid/hello\n\ngo 1.19\n";
    put(&v.join("list"), "v1.0.0\n");
    put(&v.join("v1.0.0.info"), "{\"Version\":\"v1.0.0\",\"Time\":\"2026-10-01T00:00:00Z\"}\n");
    put(&v.join("v1.0.0.mod"), module);
    let at = "m5.example.invalid/hello@v1.0.0";
    let (gomod, hello) = (format!("{at}/go.mod"), format!("{at}/hello.go"));
    zip(
        python,
        &v.join("v1.0.0.zip"),
        &[(&gomod, module), (&hello, "package hello\n\nfunc Hello() string { return \"m5\" }\n")],
    );
    // Gradle: a Maven repository of one artifact
    let d = repo.join("maven/org/example/m5/dep/1.0");
    put(
        &d.join("dep-1.0.pom"),
        "<project><modelVersion>4.0.0</modelVersion><groupId>org.example.m5</groupId>\
         <artifactId>dep</artifactId><version>1.0</version></project>\n",
    );
    put(&d.join("dep-1.0.jar"), "a stand-in, never opened");
}

/// The plain HTTP registry: every request's head read, said (`GET /path`) in `served`, answered with the file
/// under `repo` it names (404 for none; a path with `..` is none) and the connection closed — on `ip`, its address.
fn serve_plain(repo: PathBuf, ip: IpAddr, served: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let l = std::net::TcpListener::bind((ip, 0)).unwrap();
    let at = l.local_addr().unwrap();
    crate::event::thread("tarae-m5-plain", move || {
        for mut s in l.incoming().map_while(Result::ok) {
            let (repo, served) = (repo.clone(), served.clone());
            crate::event::thread("tarae-m5-plain", move || {
                let mut head = Vec::new();
                let mut b = [0u8; 4096];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 65_536 {
                    match s.read(&mut b) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => head.extend_from_slice(&b[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&head);
                let mut first = text.lines().next().unwrap_or_default().split(' ');
                let (method, path) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
                served.lock().unwrap().push(format!("{method} {path}"));
                let file = path.strip_prefix('/').filter(|p| !p.split('/').any(|c| c == ".."));
                let body = file.and_then(|f| std::fs::read(repo.join(f)).ok());
                let reply = match &body {
                    Some(b) => {
                        format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", b.len())
                    }
                    None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
                };
                let _ = s.write_all(reply.as_bytes());
                if let (Some(b), false) = (body, method == "HEAD") {
                    let _ = s.write_all(&b);
                }
            });
        }
    });
    at
}

/// The test's registries, made once for every test in the process: its folder, what they served, and the HTTPS
/// one's stdin (held: it ends with the process) — None where no JDK is here (the plain one alone).
struct Registries {
    base: PathBuf,
    served: Arc<Mutex<Vec<String>>>,
    tls: Option<Mutex<std::process::ChildStdin>>,
}

static REGISTRIES: OnceLock<Option<Registries>> = OnceLock::new();

/// The registries — None (said why by the caller) where this machine has no address beyond loopback.
fn registries() -> Option<&'static Registries> {
    REGISTRIES
        .get_or_init(|| {
            let base = std::env::temp_dir().join(format!("tarae-m5-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            let base = std::fs::canonicalize(&base).unwrap();
            let ip = outside_at(&base)?;
            let served = Arc::new(Mutex::new(Vec::new()));
            plain_registry(&base.join("plain"), &base.join("scratch"));
            let at = serve_plain(base.join("plain"), ip, served.clone());
            // (the names locked only to add them: other tests' runs resolve through them meanwhile)
            super::TEST_NAMES.lock().unwrap().extend(PLAIN.map(|t| (host(t), at)));
            let tls = match (on_path("java"), on_path("keytool"), on_path("python3")) {
                (Some(java), Some(kt), Some(python)) => {
                    certificate(&base, &kt);
                    tls_registry(&base.join("repo"), &base.join("scratch"), &python);
                    let (port, requests, up) = serve(&base, &java, ip, 1800);
                    let into = served.clone();
                    crate::event::thread("tarae-m5-tls", move || {
                        requests.iter().for_each(|r| into.lock().unwrap().push(r))
                    });
                    super::TEST_NAMES
                        .lock()
                        .unwrap()
                        .extend(TLS.map(|t| (host(t), SocketAddr::new(ip, port))));
                    Some(Mutex::new(up))
                }
                _ => None,
            };
            Some(Registries { base, served, tls })
        })
        .as_ref()
}

/// One tool's run: its program, arguments and environment, run in its project; what the registry must have
/// served it and what it must have made there.
struct Asked {
    program: PathBuf,
    args: Vec<String>,
    env: Vec<(&'static str, String)>,
    served: &'static str,
    made: &'static str,
}

/// `tool`'s project at `root` (made here, outside the sandbox: its build files are protected names), and the run
/// that fetches its package — trusting the HTTPS registry by the tool's own setting, its homes in the project.
fn project(tool: &str, program: PathBuf, root: &Path, base: &Path) -> Asked {
    std::fs::create_dir_all(root).unwrap();
    if TLS.contains(&tool) {
        // (the trust in the project, where a run reads it)
        for f in ["repo.pem", "trust.p12"] {
            put(&root.join(f), std::fs::read(base.join(f)).unwrap());
        }
    }
    let pem = root.join("repo.pem").display().to_string();
    let at = |p: &str| root.join(p).display().to_string();
    let h = host(tool);
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let (args, env, served, made) = match tool {
        "curl" => (
            args(&["-sS", "-o", &at("curl.txt"), &format!("http://{h}/curl.txt")]),
            Vec::new(),
            "GET /curl.txt",
            "curl.txt",
        ),
        "cargo" => {
            put(
                &root.join("Cargo.toml"),
                // (a workspace of its own: it's under tarae's tree)
                "[package]\nname = \"m5c\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nmfive = \"1\"\n\n\
                 [workspace]\n",
            );
            put(&root.join("src/main.rs"), "fn main() {}\n");
            // (there before the run, as a user's is: a folder holding a protected name gets no new entries)
            std::fs::create_dir_all(root.join("cargo-home")).unwrap();
            let registry = format!("source.m5.registry=\"sparse+http://{h}/cargo/\"");
            let replace = "source.crates-io.replace-with=\"m5\"";
            (
                args(&["fetch", "--config", replace, "--config", &registry]),
                vec![("CARGO_HOME", at("cargo-home"))],
                "GET /cargo/dl/mfive/1.0.0/download",
                "Cargo.lock",
            )
        }
        "npm" => (
            args(&[
                "install",
                "m5pad",
                &format!("--registry=https://{h}/npm/"),
                "--prefix",
                &at("npm"),
                "--cache",
                &at("npm-cache"),
                "--cafile",
                &pem,
                "--no-save",
                "--no-package-lock",
                "--no-audit",
                "--no-fund",
            ]),
            Vec::new(),
            "GET /npm/tarballs/m5pad-1.0.0.tgz",
            "npm/node_modules/m5pad/package.json",
        ),
        "pip" => (
            args(&[
                "-m",
                "pip",
                "download",
                "--no-deps",
                "--no-cache-dir",
                "--disable-pip-version-check",
                "-d",
                &at("pipd"),
                "--index-url",
                &format!("https://{h}/pypi/simple"),
                "--cert",
                &pem,
                "m5six",
            ]),
            Vec::new(),
            "GET /pypi/files/m5six-1.0-py3-none-any.whl",
            "pipd/m5six-1.0-py3-none-any.whl",
        ),
        "go" => {
            put(&root.join("go.mod"), "module m5go\n\ngo 1.19\n");
            (
                args(&["get", "m5.example.invalid/hello@v1.0.0"]),
                vec![
                    ("GOPROXY", format!("https://{h}/go")),
                    ("GOSUMDB", "off".into()),
                    ("GOPATH", at("gopath")),
                    ("GOMODCACHE", at("gopath/pkg/mod")),
                    ("GOFLAGS", "-modcacherw".into()),
                    ("SSL_CERT_FILE", pem),
                ],
                "GET /go/m5.example.invalid/hello/@v/v1.0.0.zip",
                "gopath/pkg/mod/m5.example.invalid/hello@v1.0.0/hello.go",
            )
        }
        "gradle" => {
            let trust = at("trust.p12");
            put(&root.join("settings.gradle"), "rootProject.name = 'm5'\n");
            put(
                &root.join("build.gradle"),
                format!(
                    "repositories {{ maven {{ url = 'https://{h}/maven/' }} }}\n\
                     configurations {{ fetched }}\n\
                     dependencies {{ fetched 'org.example.m5:dep:1.0' }}\n\
                     tasks.register('fetch', Copy) {{ from configurations.fetched; into layout.buildDirectory.dir('fetched') }}\n"
                ),
            );
            put(
                &root.join("gradle.properties"),
                format!(
                    "systemProp.javax.net.ssl.trustStore={trust}\nsystemProp.javax.net.ssl.trustStorePassword={STORE}\n\
                     systemProp.javax.net.ssl.trustStoreType=PKCS12\n"
                ),
            );
            (
                args(&["--console=plain", "fetch"]),
                Vec::new(),
                "GET /maven/org/example/m5/dep/1.0/dep-1.0.jar",
                "build/fetched/dep-1.0.jar",
            )
        }
        _ => unreachable!("{tool}"),
    };
    Asked { program, args, env, served, made }
}

/// `tool` fetches its package as Claude's runs do — the network open, its registry named — by the run's way out:
/// where the session takes its connections, no proxy URL, no proxy variable and no proxy line in its Gradle home
/// (and on a machine said to give TPROXY, `TARAE_EXPECT_NETWORK`, that way it must be); elsewhere the URL. Skips
/// itself (said) where its tool, a JDK for the HTTPS registry, Landlock or the supervisor isn't here.
fn fetches(tool: &str) {
    let test = format!("m5_tests::m5_{tool}");
    let program = match tool {
        "cargo" => {
            on_path("rustup").map(|_| PathBuf::from(setup(Command::new("rustup").args(["which", "cargo"]))))
        }
        "pip" => on_path("python3"),
        t => on_path(t),
    };
    let Some(program) = program else { return skip(&test, &format!("no {tool} on PATH")) };
    if crate::landlock::abi().is_ok_and(|n| n < 2) || crate::landlock::abi().is_err() {
        return skip(&test, "no Landlock tarae uses");
    }
    if let Some(why) = super::super::supervisor::unavailable() {
        return skip(&test, &why);
    }
    let Some(r) = registries() else { return skip(&test, "no address of this machine's beyond loopback") };
    if TLS.contains(&tool) && r.tls.is_none() {
        return skip(&test, "no java, keytool and python3 on PATH (its HTTPS registry's)");
    }
    let root =
        std::env::current_dir().unwrap().join(format!("target/tarae-m5-{tool}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let asked = project(tool, program, &root, &r.base);
    // (under the build's `target/`, not `/tmp`: a run writes there)
    let root = std::fs::canonicalize(&root).unwrap();
    let rule = super::egress::Pattern::parse(&host(tool)).unwrap();
    let llm = crate::llm::LlmConfig {
        sandbox_network: true,
        sandbox_hosts: Some(vec![rule]),
        sandbox_localhost: true,
        ..Default::default()
    };
    // (7b: its trust files are named like keys, `*.pem`, `*.p12` — closed to a run until the user opens each from
    // the run's panel, `allow::Grant::Key`, as a project with them would)
    let open = ["repo.pem", "trust.p12"].map(|f| root.join(f)).into_iter().filter(|f| f.exists());
    let mut p = crate::sandbox::policy_opening(&root, &llm, &open.collect::<Vec<_>>()).unwrap();
    // (this machine's own address stands for the outside: a policy that knows none)
    p.own_addresses = Some(Vec::new());
    p.watch = crate::sandbox::watch_for(&p);
    let mut cmd = crate::sandbox::command(Some(&p), &asked.program.to_string_lossy(), &asked.args);
    cmd.current_dir(&root).envs(asked.env.iter().map(|(k, v)| (k, v)));
    // (a variable naming a proxy — but Go's registry, the test's own)
    let vars: Vec<String> = cmd
        .get_envs()
        .filter(|(k, v)| v.is_some() && k.to_string_lossy().to_ascii_lowercase().contains("proxy"))
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .filter(|k| k != "GOPROXY")
        .collect();
    let gradle_lines = p.env.iter().filter(|(k, _)| k == "GRADLE_USER_HOME").any(|(_, home)| {
        std::fs::read_to_string(Path::new(home).join("gradle.properties"))
            .is_ok_and(|t| t.contains("proxyHost"))
    });
    let url = p.proxy_url().is_some();
    let ended = crate::sandbox::after_run(Some(&p));
    let (status, out, err) = finish(crate::sandbox::spawn(Some(&p), &cmd).expect("the tool starts"), 300);
    ended();
    let lines: Vec<(bool, String)> = out
        .lines()
        .map(|l| (false, l.to_string()))
        .chain(err.lines().map(|l| (true, l.to_string())))
        .collect();
    let report = crate::sandbox::Guard::In(Box::new(p.clone())).report(&lines);
    if let Some(Ok(c)) = &p.confine
        && let Some(k) = &c.keeper
    {
        k.end();
    }
    let served = r.served.lock().unwrap().clone();
    let said = format!("{out}\n{err}\nreport: {report}\nserved: {served:?}");
    let way = format!(
        "redirected {} · URL {url} · variables {vars:?} · Gradle's lines {gradle_lines}",
        p.redirected
    );
    if std::env::var("TARAE_EXPECT_NETWORK").is_ok_and(|n| n == "tproxy") {
        assert!(p.redirected, "{tool}: TPROXY expected here — {way}");
    }
    match p.redirected {
        true => assert!(!url && vars.is_empty() && !gradle_lines, "{tool}: {way}"),
        false => assert!(url && !vars.is_empty(), "{tool}: its way out is the URL — {way}"),
    }
    assert!(status.is_some_and(|s| s.success()), "{tool} ({way}): {status:?}\n{said}");
    assert!(served.iter().any(|s| s == asked.served), "{tool}: {} served\n{said}", asked.served);
    assert!(root.join(asked.made).exists(), "{tool}: {} made\n{said}", asked.made);
    std::fs::remove_dir_all(&root).ok();
}

#[test]
fn m5_curl() {
    fetches("curl")
}

#[test]
fn m5_cargo() {
    fetches("cargo")
}

#[test]
fn m5_npm() {
    fetches("npm")
}

#[test]
fn m5_pip() {
    fetches("pip")
}

#[test]
fn m5_go() {
    fetches("go")
}

#[test]
fn m5_gradle() {
    fetches("gradle")
}
