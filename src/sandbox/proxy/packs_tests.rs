//! The packs' proxy lines through the real path, on both systems (step 5f, `target/sb/step5f-plan.md`): a run's
//! policy as `sandbox::policy` makes it (its run homes, its proxy files), its environment (`command`), its start
//! and end (`spawn`, `after_run`) — macOS through the run's own port and Seatbelt, Linux through the supervisor.
//! Nothing leaves this machine: the outside is the test's own HTTPS server under an `example.invalid` name
//! (`TEST_NAMES`) — on loopback, taken for beyond (macOS, `TEST_BEYOND`), or on this machine's own address with a
//! policy that knows none (Linux) — with a certificate made for it and trusted by the test's project alone.

use std::io::{BufRead, Read};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A run's time limit: past `secs`, its process group — the run's own, a session of its own (`spawn::setsid`)
/// that this test started — is killed, so its output ends and a run that hangs fails its test instead of
/// hanging it. Dropped once the run is reaped.
pub(super) struct Deadline(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Deadline {
    pub(super) fn new(pid: u32, secs: u64) -> Deadline {
        use std::sync::atomic::Ordering::SeqCst;
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = done.clone();
        crate::event::thread("tarae-5d-test", move || {
            let by = Instant::now() + Duration::from_secs(secs);
            while !seen.load(SeqCst) && Instant::now() < by {
                std::thread::sleep(Duration::from_millis(50));
            }
            if !seen.load(SeqCst) {
                eprintln!("the run (pid {pid}) passed its {secs} s: its process group killed");
                // SAFETY: a signal to the process group of a run this test started, not reaped yet.
                unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            }
        });
        Deadline(done)
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// What a process started in a session of its own (`spawn::setsid`, `sandbox::spawn`) printed, both streams, and
/// how it ended — read to its end within `secs` (`Deadline`).
pub(super) fn finish(
    s: crate::spawn::Spawned,
    secs: u64,
) -> (Option<std::process::ExitStatus>, String, String) {
    let limit = Deadline::new(s.pid, secs);
    let mut err = s.stderr.expect("its stderr");
    let errs = crate::event::thread("tarae-5d-test", move || {
        let mut e = String::new();
        let _ = err.read_to_string(&mut e);
        e
    });
    let mut out = String::new();
    let _ = s.stdout.expect("its stdout").read_to_string(&mut out);
    let status = crate::spawn::wait(s.pid).ok();
    drop(limit);
    (status, out, errs.join().unwrap_or_default())
}

/// The repository's name, as the run's build names it.
const HOST: &str = "repo-5f.example.invalid";

/// The stores' password — the test's own key pair and trust store, nothing else's.
const STORE: &str = "tarae-fake-store";

/// The test's HTTPS server: `java Repo.java <address> <keystore> <password> <folder> <seconds>` serves the files
/// under the folder, says each request on a line (`GET /path`) and its port first (`port N`), and ends itself
/// when its stdin closes (the test is over, passed or not) or after its seconds.
const REPO_JAVA: &str = r#"
import com.sun.net.httpserver.*;
import javax.net.ssl.*;
import java.io.*;
import java.net.*;
import java.nio.file.*;
import java.security.KeyStore;

public class Repo {
    public static void main(String[] a) throws Exception {
        char[] pw = a[2].toCharArray();
        KeyStore ks = KeyStore.getInstance("PKCS12");
        try (InputStream in = Files.newInputStream(Path.of(a[1]))) { ks.load(in, pw); }
        KeyManagerFactory kmf = KeyManagerFactory.getInstance(KeyManagerFactory.getDefaultAlgorithm());
        kmf.init(ks, pw);
        SSLContext ssl = SSLContext.getInstance("TLS");
        ssl.init(kmf.getKeyManagers(), null, null);
        HttpsServer s = HttpsServer.create(new InetSocketAddress(InetAddress.getByName(a[0]), 0), 0);
        s.setHttpsConfigurator(new HttpsConfigurator(ssl));
        Path root = Path.of(a[3]).toAbsolutePath().normalize();
        s.createContext("/", x -> {
            String p = x.getRequestURI().getPath();
            System.out.println(x.getRequestMethod() + " " + p);
            System.out.flush();
            Path f = root.resolve(p.substring(1)).normalize();
            byte[] body = f.startsWith(root) && Files.isRegularFile(f) ? Files.readAllBytes(f) : null;
            if (body == null) {
                x.sendResponseHeaders(404, -1);
            } else if (x.getRequestMethod().equals("HEAD")) {
                x.sendResponseHeaders(200, -1);
            } else {
                x.sendResponseHeaders(200, body.length);
                x.getResponseBody().write(body);
            }
            x.close();
        });
        s.start();
        System.out.println("port " + s.getAddress().getPort());
        System.out.flush();
        Thread ended = new Thread(() -> {
            try { while (System.in.read() >= 0) {} } catch (IOException e) {}
            System.exit(0);
        });
        ended.setDaemon(true);
        ended.start();
        Thread.sleep(Long.parseLong(a[4]) * 1000);
        System.exit(0);
    }
}
"#;

/// A program on `PATH` — None: not there.
pub(super) fn on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).map(|d| d.join(name)).find(|p| p.is_file())
}

/// The test says it skips itself, and why (`test`: its name in this module, else its path from `proxy::`).
pub(super) fn skip(test: &str, why: &str) {
    let path = if test.contains("::") { test.to_string() } else { format!("packs_tests::{test}") };
    let line = format!("{}sandbox::proxy::{path} — {why}\n", crate::test_results::SELF_SKIP);
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
}

/// Where the test's outside listens: loopback, taken for beyond in the runs of `root` (macOS) — or this
/// machine's own address, beyond for a policy that knows none (Linux) — None where there's none.
#[cfg(not(target_os = "linux"))]
fn outside_at(root: &Path) -> Option<IpAddr> {
    let lo = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    super::TEST_BEYOND.lock().unwrap().push((root.to_path_buf(), lo));
    Some(lo)
}

#[cfg(target_os = "linux")]
fn outside_at(_root: &Path) -> Option<IpAddr> {
    // (a connected UDP socket's own address: no packet is sent)
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    Some(s.local_addr().ok()?.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

/// `keytool` with `args`, its failure the test's.
fn keytool(keytool: &Path, args: &[&str]) {
    let out = Command::new(keytool).args(args).output().unwrap();
    assert!(out.status.success(), "keytool {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// The test's repository: `base/repo` holding one artifact (`org.example.tarae5f:dep:1.0`), a key pair for
/// `HOST` (`base/server.p12`) and a trust store holding only its certificate (`trust`: in the project, where a
/// run reads it).
fn repository(base: &Path, trust: &Path, kt: &Path) {
    let dir = base.join("repo/org/example/tarae5f/dep/1.0");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("dep-1.0.pom"),
        "<project><modelVersion>4.0.0</modelVersion><groupId>org.example.tarae5f</groupId>\
         <artifactId>dep</artifactId><version>1.0</version></project>\n",
    )
    .unwrap();
    std::fs::write(dir.join("dep-1.0.jar"), "a stand-in, never opened").unwrap();
    let (server, cert) = (base.join("server.p12"), base.join("repo.cer"));
    let p = |p: &Path| p.display().to_string();
    let dname = format!("CN={HOST}");
    let san = format!("san=dns:{HOST}");
    keytool(
        kt,
        &[
            "-genkeypair",
            "-alias",
            "repo",
            "-keyalg",
            "EC",
            "-groupname",
            "secp256r1",
            "-validity",
            "2",
            "-dname",
            &dname,
            "-ext",
            &san,
            "-keystore",
            &p(&server),
            "-storepass",
            STORE,
            "-storetype",
            "PKCS12",
        ],
    );
    keytool(
        kt,
        &["-exportcert", "-alias", "repo", "-keystore", &p(&server), "-storepass", STORE, "-file", &p(&cert)],
    );
    keytool(
        kt,
        &[
            "-importcert",
            "-noprompt",
            "-alias",
            "repo",
            "-file",
            &p(&cert),
            "-keystore",
            &p(trust),
            "-storepass",
            STORE,
            "-storetype",
            "PKCS12",
        ],
    );
}

/// The test's HTTPS server for `base`'s repository on `ip`, up for `secs` at most: its port, the lines it says
/// (the requests) as they come, and its stdin — dropped, the server ends (a test that fails too: the drop comes
/// as it unwinds).
fn serve(
    base: &Path,
    java: &Path,
    ip: IpAddr,
    secs: u64,
) -> (u16, std::sync::mpsc::Receiver<String>, std::process::ChildStdin) {
    std::fs::write(base.join("Repo.java"), REPO_JAVA).unwrap();
    let mut c = Command::new(java);
    c.arg(base.join("Repo.java")).arg(ip.to_string()).arg(base.join("server.p12")).arg(STORE);
    c.arg(base.join("repo")).arg(secs.to_string()).stdin(Stdio::piped()).stdout(Stdio::piped());
    let mut child = c.stderr(Stdio::inherit()).spawn().unwrap();
    let up = child.stdin.take().unwrap();
    let out = std::io::BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    crate::event::thread("tarae-5f-repo", move || {
        for l in out.lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
        let _ = child.wait();
    });
    let first = rx.recv_timeout(Duration::from_secs(60)).expect("the test's server started");
    let port = first.strip_prefix("port ").and_then(|p| p.parse().ok()).expect(&first);
    (port, rx, up)
}

/// The JVM reads no HTTPS_PROXY (step 5f, M2): with the Java pack's lines in the run's Gradle home, a Gradle
/// build resolves a dependency from an HTTPS repository through tarae's proxy — its own HTTP client sending the
/// run's credentials where the URL has them — and the build gets the file; without those lines it goes direct
/// and the sandbox refuses it. The repository is the test's own server; its certificate is trusted by the
/// test's project alone (its `gradle.properties`' trust store). Skips without `gradle`, `java` and `keytool`
/// on `PATH`, where no sandbox can start here, and where this machine has no address to stand for the outside.
#[test]
fn gradle_resolves_through_the_proxy() {
    const TEST: &str = "gradle_resolves_through_the_proxy";
    let (Some(gradle), Some(java), Some(kt)) = (on_path("gradle"), on_path("java"), on_path("keytool"))
    else {
        return skip(TEST, "no gradle, java and keytool on PATH");
    };
    #[cfg(not(target_os = "linux"))]
    if crate::sandbox::skipped_inside(TEST) {
        return;
    }
    #[cfg(target_os = "linux")]
    if crate::landlock::abi().is_ok_and(|n| n < 2) || crate::landlock::abi().is_err() {
        return skip(TEST, "no Landlock tarae uses");
    }
    #[cfg(target_os = "linux")]
    if let Some(why) = super::super::supervisor::unavailable() {
        return skip(TEST, &why);
    }
    // (under the build's `target/`, not `/tmp`: a run writes there)
    let base =
        std::env::current_dir().unwrap().join(format!("target/tarae-5f-gradle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("proj")).unwrap();
    let base = std::fs::canonicalize(&base).unwrap();
    let root = base.join("proj");
    let Some(ip) = outside_at(&root) else {
        return skip(TEST, "no address of this machine's beyond loopback");
    };
    let trust = root.join("trust.p12");
    repository(&base, &trust, &kt);
    // (`_up` held to the test's end: the server ends with it)
    let (port, requests, _up) = serve(&base, &java, ip, 300);
    super::TEST_NAMES.lock().unwrap().push((HOST.into(), std::net::SocketAddr::new(ip, port)));
    let trust = trust.display().to_string();
    for (f, text) in [
        ("settings.gradle", "rootProject.name = 'fetching'\n".to_string()),
        (
            "build.gradle",
            format!(
                "repositories {{ maven {{ url = 'https://{HOST}:{port}/' }} }}\n\
                 configurations {{ fetched }}\n\
                 dependencies {{ fetched 'org.example.tarae5f:dep:1.0' }}\n\
                 tasks.register('fetch', Copy) {{ from configurations.fetched; into layout.buildDirectory.dir('fetched') }}\n"
            ),
        ),
        (
            "gradle.properties",
            format!(
                "systemProp.javax.net.ssl.trustStore={trust}\nsystemProp.javax.net.ssl.trustStorePassword={STORE}\n\
                 systemProp.javax.net.ssl.trustStoreType=PKCS12\n"
            ),
        ),
    ] {
        std::fs::write(root.join(f), text).unwrap();
    }
    // The run as Claude's are: the network open through the proxy, this one host named at its port (its daemon
    // reached on this machine's ports)
    let host = super::egress::Pattern::parse(&format!("{HOST}:{port}")).unwrap();
    let llm = crate::llm::LlmConfig {
        sandbox_network: true,
        sandbox_hosts: Some(vec![host]),
        sandbox_localhost: true,
        ..Default::default()
    };
    #[allow(unused_mut)]
    let mut p = crate::sandbox::policy(&root, &llm, &[]).unwrap();
    #[cfg(target_os = "linux")]
    {
        // (this machine's own address stands for the outside: a policy that knows none)
        p.own_addresses = Some(Vec::new());
    }
    p.watch = crate::sandbox::watch_for(&p);
    assert!(p.proxy_url().is_some(), "the run has a proxy URL");
    let args = ["--console=plain", "--stacktrace", "fetch"].map(String::from);
    let mut cmd = crate::sandbox::command(Some(&p), &gradle.to_string_lossy(), &args);
    cmd.current_dir(&root);
    let ended = crate::sandbox::after_run(Some(&p));
    let (status, out, err) = finish(crate::sandbox::spawn(Some(&p), &cmd).expect("gradle starts"), 300);
    ended();
    let said = format!("{out}\n{err}");
    let seen: Vec<String> = requests.try_iter().collect();
    let blocked = p.watch.as_ref().map(|w| w.blocked()).unwrap_or_default();
    #[cfg(target_os = "linux")]
    if let Some(Ok(c)) = &p.confine
        && let Some(k) = &c.keeper
    {
        k.end();
    }
    assert!(status.is_some_and(|s| s.success()), "{said}\nblocked: {blocked:?}\nserved: {seen:?}");
    for f in ["dep-1.0.pom", "dep-1.0.jar"] {
        let asked = format!("GET /org/example/tarae5f/dep/1.0/{f}");
        assert!(seen.contains(&asked), "{f} through the proxy: {seen:?}\n{said}");
    }
    assert!(root.join("build/fetched/dep-1.0.jar").is_file(), "{said}");
    // (the proxy refused nothing, and nothing went direct — macOS's resolver denial is Q4's, not this test's)
    let ops = [super::OP, super::HOST_OP, "network-direct"];
    let refused: Vec<_> = blocked.seen.iter().filter(|d| ops.contains(&d.op.as_str())).collect();
    assert!(refused.is_empty(), "{refused:?}");
    std::fs::remove_dir_all(&base).ok();
}
