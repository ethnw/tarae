//! A tool a pack has tarae download itself — an `[install.<tool>]` way with `download` (`packs.rs`; the
//! format in docs/languages.md, "Downloads"): its newest release found as the pack says, checked against the
//! SHA-256 published beside it — or the release the pack pins, against the SHA-256 (and size) it pins —
//! unpacked into `<data>/<tool>` and found there from then on (`installed`).
//! Language-neutral: what to fetch, from where, and what it runs on are all the pack's.
//!
//! Shapes it covers: a release whose version is read off a page (a folder listing, `latest.txt`, a GitHub
//! release's JSON — the highest of what a regex picks), whose file name is a template or read off a page too,
//! published as a `.tar.gz` or as the program itself, with a checksum file beside it — one hash, or
//! `<hash>  <name>` lines (`sha256sums.txt`, helm-ls's `<binary>.sha256sum`). Not: zip archives, releases without
//! a published checksum (lua-language-server's).

use std::path::{Path, PathBuf};

/// How the newest of something is found: the page at `from` (a template), and a regex whose first group (else
/// the whole match) is a candidate — the highest one wins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pick {
    pub from: String,
    pub pick: String,
}

/// Which release: one the pack pins (a newer one is a pack change), or the newest one found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Version {
    Pinned(String),
    Newest(Pick),
}

/// What the file is checked against: a SHA-256 the pack pins (with a pinned version — java-debug's: Maven
/// Central publishes only SHA-1 for it), or the one published beside it (a template: where).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sum {
    Pinned(String),
    Published(String),
}

/// The file's name: a template, or read off a page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Name {
    Is(String),
    Pick(Pick),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unpack {
    /// A gzipped tar, unpacked with `tar`.
    TarGz,
    /// The download is the program.
    File,
}

/// What the program runs on, at least which version — looked at before the card (`java -version`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Runtime {
    pub program: String,
    /// A variable naming its home (`JAVA_HOME`): `<home>/bin/<program>` before `PATH`'s.
    pub home: Option<String>,
    /// What to run it with to learn its version.
    pub ask: Vec<String>,
    /// A regex whose first group is the major version in what it says.
    pub version: String,
    pub at_least: u64,
    /// How to get one, said when there's none or it's older.
    pub get: String,
}

/// A pack's `download` way.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Download {
    /// Where the file is — a template (`{version}`, `{file}`, `{os}`, `{arch}`).
    pub url: String,
    pub version: Version,
    pub file: Name,
    pub sha256: Sum,
    /// Its size in bytes, when the pack pins it too.
    pub size: Option<u64>,
    pub unpack: Unpack,
    /// The program, inside what's unpacked — a template, relative.
    pub program: String,
    /// Programs it needs on PATH besides (a launcher's interpreter).
    pub needs: Vec<String>,
    pub runtime: Option<Runtime>,
    /// `{os}` and `{arch}` as this release names them: Rust's (`linux`, `macos`, `x86_64`, `aarch64`) → its own.
    pub rename: Vec<(String, String)>,
}

/// The placeholders a template may hold.
const PLACEHOLDERS: [&str; 4] = ["version", "file", "os", "arch"];

/// Where a download records the program it unpacked (relative to its folder).
const PROGRAM_MARK: &str = ".tarae-program";

/// A template's placeholders are all known ones.
pub fn check_template(t: &str) -> Result<(), String> {
    let mut rest = t;
    while let Some(i) = rest.find('{') {
        let end = rest[i..].find('}').ok_or_else(|| format!("`{t}`: a `{{` isn't closed"))? + i;
        let name = &rest[i + 1..end];
        if !PLACEHOLDERS.contains(&name) {
            return Err(format!(
                "`{t}`: no such placeholder `{{{name}}}` (there are {})",
                PLACEHOLDERS.join(", ")
            ));
        }
        rest = &rest[end + 1..];
    }
    Ok(())
}

/// A version or file name as a template may take it: no path in it, nothing a URL or a shell reads apart.
fn plain(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.') && s.chars().all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
}

/// `1.61.0` → [1, 61, 0] — for the highest; letters between count as separators.
fn version_key(v: &str) -> Vec<u64> {
    v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().unwrap_or(0)).collect()
}

/// The highest of what `pick` finds in `text` — each a plain name, or an error saying what it found.
fn highest(text: &str, pick: &str, at: &str) -> Result<String, String> {
    let re = regex::Regex::new(pick).map_err(|e| format!("`{pick}`: {e}"))?;
    let found: Vec<&str> =
        re.captures_iter(text).filter_map(|c| c.get(1).or_else(|| c.get(0))).map(|m| m.as_str()).collect();
    if let Some(bad) = found.iter().find(|f| !plain(f)) {
        let shown: String = bad.chars().take(60).collect();
        return Err(format!("{at} names something that isn't a plain name: {shown:?}"));
    }
    found
        .into_iter()
        .max_by_key(|f| version_key(f))
        .map(str::to_string)
        .ok_or_else(|| format!("nothing found at {at}"))
}

/// A SHA-256 as hex: 64 hex digits.
pub fn is_sha256(h: &str) -> bool {
    h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit())
}

/// The published checksum of `file`: the only hash there, or the one on `file`'s line (`<hash>  <name>`,
/// `<hash> *<name>`).
fn checksum_of(published: &str, file: &str) -> Option<String> {
    let hex = is_sha256;
    let rows: Vec<Vec<&str>> = published
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>())
        .filter(|r| !r.is_empty())
        .collect();
    let named = rows.iter().find(|r| r.len() == 2 && r[1].trim_start_matches('*') == file && hex(r[0]));
    let alone = (rows.len() == 1 && rows[0].len() == 1 && hex(rows[0][0])).then(|| &rows[0]);
    named.or(alone).map(|r| r[0].to_ascii_lowercase())
}

/// The values a template is filled with.
struct Vars {
    version: String,
    file: String,
    os: String,
    arch: String,
}

impl Vars {
    fn new(d: &Download) -> Vars {
        let named = |s: &str| d.rename.iter().find(|(a, _)| a == s).map_or(s.to_string(), |(_, b)| b.clone());
        Vars {
            version: String::new(),
            file: String::new(),
            os: named(std::env::consts::OS),
            arch: named(std::env::consts::ARCH),
        }
    }

    fn fill(&self, t: &str) -> String {
        t.replace("{version}", &self.version)
            .replace("{file}", &self.file)
            .replace("{os}", &self.os)
            .replace("{arch}", &self.arch)
    }
}

impl Download {
    /// It can be taken here: a downloader, `tar` for an archive, and what it `needs`, on PATH (`has`).
    pub fn fits(&self, has: impl Fn(&str) -> bool) -> bool {
        (has("curl") || has("wget"))
            && (self.unpack == Unpack::File || has("tar"))
            && self.needs.iter().all(|n| has(n))
    }

    /// Where it comes from: the download's host.
    pub fn host(&self) -> &str {
        let rest = self.url.split_once("://").map_or(self.url.as_str(), |(_, r)| r);
        rest.split('/').next().unwrap_or(rest)
    }

    /// How it's said where no way fits: "a download from <host> (with curl or wget, tar, python3)".
    pub fn shown(&self) -> String {
        let mut with = vec!["curl or wget".to_string()];
        if self.unpack == Unpack::TarGz {
            with.push("tar".into());
        }
        with.extend(self.needs.iter().cloned());
        format!("a download from {} (with {})", self.host(), with.join(", "))
    }

    /// What its card says it fetches and how it's checked: "version 0.53.1 from repo1.maven.org — checked
    /// against the SHA-256 its pack pins", "its newest release from … — checked against its published
    /// SHA-256".
    pub fn said(&self) -> String {
        let which = match &self.version {
            Version::Pinned(v) => format!("version {v}"),
            Version::Newest(_) => "its newest release".into(),
        };
        let checked = match &self.sha256 {
            Sum::Pinned(_) => "the SHA-256 its pack pins",
            Sum::Published(_) => "its published SHA-256",
        };
        format!("{which} from {} — checked against {checked}", self.host())
    }
}

/// Download one URL — curl, else wget (both honor `https_proxy` for corporate proxies).
pub fn http_get(url: &str) -> Result<Vec<u8>, String> {
    let run = |cmd: &str, args: &[&str]| std::process::Command::new(cmd).args(args).output();
    let out = match run("curl", &["-fsSL", "--retry", "2", url]) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            run("wget", &["-qO-", url]).map_err(|_| "needs curl or wget".to_string())?
        }
        r => r.map_err(|e| format!("curl: {e}"))?,
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().last().unwrap_or("download failed").trim().to_string());
    }
    Ok(out.stdout)
}

fn text(url: &str) -> Result<String, String> {
    http_get(url).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// What a download put in place: its version, and the program.
#[derive(Debug, PartialEq)]
pub struct Got {
    pub version: String,
    pub program: PathBuf,
}

/// Downloads the newest `d` into `dir` (on a worker): found, checked against its published checksum, unpacked,
/// and only then put in place of what's there.
pub fn fetch(d: &Download, dir: &Path) -> Result<Got, String> {
    let mut vars = Vars::new(d);
    vars.version = match &d.version {
        Version::Pinned(v) if plain(v) => v.clone(),
        Version::Pinned(v) => return Err(format!("the pinned version isn't a plain one: {v:?}")),
        Version::Newest(p) => {
            let from = vars.fill(&p.from);
            highest(&text(&from)?, &p.pick, &from)?
        }
    };
    vars.file = match &d.file {
        Name::Is(t) => vars.fill(t),
        Name::Pick(p) => {
            let from = vars.fill(&p.from);
            highest(&text(&from)?, &p.pick, &from)?
        }
    };
    let file = vars.file.clone();
    if !plain(&file) {
        return Err(format!("the file's name isn't a plain one: {file:?}"));
    }
    let (sum, whose) = match &d.sha256 {
        Sum::Pinned(h) => (h.to_ascii_lowercase(), "the checksum its pack pins"),
        Sum::Published(at) => {
            let published =
                text(&vars.fill(at)).map_err(|e| format!("no checksum published for {file} ({e})"))?;
            let sum = checksum_of(&published, &file)
                .ok_or_else(|| format!("no checksum for {file} in what's published"))?;
            (sum, "its published checksum")
        }
    };
    let data = http_get(&vars.fill(&d.url))?;
    if let Some(size) = d.size.filter(|&s| s != data.len() as u64) {
        return Err(format!("{file} is {} bytes, not the {size} its pack says — not unpacked", data.len()));
    }
    if crate::sha256::hex(&data) != sum {
        return Err(format!("{file} doesn't match {whose} — not unpacked"));
    }
    let program = vars.fill(&d.program);
    let rel = Path::new(&program);
    if program.is_empty()
        || rel.is_absolute()
        || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(format!("the program's place isn't a plain path inside it: {program:?}"));
    }
    let parent = dir.parent().ok_or("no folder for it")?;
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let pid = std::process::id();
    let part = parent.join(format!(".{name}.part-{pid}"));
    let archive = parent.join(format!(".{name}.part-{pid}.download"));
    let _ = std::fs::remove_dir_all(&part);
    let placed = (|| {
        std::fs::create_dir_all(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        match d.unpack {
            Unpack::TarGz => {
                std::fs::write(&archive, &data).map_err(|e| format!("{}: {e}", archive.display()))?;
                let out = std::process::Command::new("tar")
                    .arg("-xzf")
                    .arg(&archive)
                    .arg("-C")
                    .arg(&part)
                    .stdin(std::process::Stdio::null())
                    .output()
                    .map_err(|e| format!("tar: {e}"))?;
                if !out.status.success() {
                    let err = String::from_utf8_lossy(&out.stderr);
                    return Err(format!("tar: {}", err.lines().last().unwrap_or("failed").trim()));
                }
            }
            Unpack::File => {
                let at = part.join(rel);
                if let Some(p) = at.parent() {
                    std::fs::create_dir_all(p).map_err(|e| format!("{}: {e}", p.display()))?;
                }
                std::fs::write(&at, &data).map_err(|e| format!("{}: {e}", at.display()))?;
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&at, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| format!("{}: {e}", at.display()))?;
            }
        }
        if !part.join(rel).is_file() {
            return Err(format!("{file} has no {program}"));
        }
        std::fs::write(part.join(PROGRAM_MARK), &program).map_err(|e| e.to_string())?;
        // What's there now goes aside first, so nothing is left half-replaced
        if dir.exists() {
            let old = parent.join(format!(".{name}.old-{pid}"));
            std::fs::rename(dir, &old).map_err(|e| format!("{}: {e}", dir.display()))?;
            let _ = std::fs::remove_dir_all(&old);
        }
        std::fs::rename(&part, dir).map_err(|e| format!("{}: {e}", dir.display()))
    })();
    let _ = std::fs::remove_file(&archive);
    if placed.is_err() {
        let _ = std::fs::remove_dir_all(&part);
    }
    placed.map(|()| Got { version: vars.version, program: dir.join(rel) })
}

/// The program a download put in `dir`, if one did (and it's there).
pub fn installed(dir: &Path) -> Option<PathBuf> {
    let rel = std::fs::read_to_string(dir.join(PROGRAM_MARK)).ok()?;
    let rel = rel.trim();
    let p = Path::new(rel);
    if rel.is_empty() || p.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
        return None;
    }
    Some(dir.join(p)).filter(|p| p.is_file())
}

/// The runtime as found here (on a worker — it runs it): its path and major version (None: couldn't tell).
pub fn probe(rt: &Runtime) -> Option<(PathBuf, Option<u64>)> {
    let on_path = || {
        std::env::var_os("PATH")
            .and_then(|p| std::env::split_paths(&p).map(|d| d.join(&rt.program)).find(|p| p.is_file()))
    };
    let at = rt
        .home
        .as_ref()
        .and_then(std::env::var_os)
        .map(|h| PathBuf::from(h).join("bin").join(&rt.program))
        .filter(|p| p.is_file())
        .or_else(on_path)?;
    let out = std::process::Command::new(&at).args(&rt.ask).stdin(std::process::Stdio::null()).output();
    let said = out.map(|o| String::from_utf8_lossy(&[o.stderr, o.stdout].concat()).into_owned()).ok();
    let version = said.and_then(|s| major(&s, &rt.version));
    Some((at, version))
}

/// The major version `pattern`'s first group finds in what a runtime said.
fn major(said: &str, pattern: &str) -> Option<u64> {
    regex::Regex::new(pattern).ok()?.captures(said)?.get(1)?.as_str().parse().ok()
}

/// Why `what` can't run here — None when the runtime found (`probe`) will do; a version it couldn't read isn't
/// held against it.
pub fn runtime_blocker(rt: &Runtime, found: Option<&(PathBuf, Option<u64>)>, what: &str) -> Option<String> {
    let (program, least) = (&rt.program, rt.at_least);
    let Some((at, version)) = found else {
        let home = rt.home.as_ref().map_or(String::new(), |h| format!("{h}, "));
        return Some(format!(
            "{what} runs on {program} {least} or newer, and there's none here ({home}PATH) — {}",
            rt.get
        ));
    };
    let v = version.filter(|v| *v < least)?;
    Some(format!("{what} runs on {program} {least} or newer — {} is {program} {v}: {}", at.display(), rt.get))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn templates_and_names() {
        assert!(check_template("https://x/{version}/{file}.sha256").is_ok());
        assert!(check_template("{os}-{arch}").is_ok());
        assert!(check_template("{verison}").unwrap_err().contains("no such placeholder"));
        assert!(check_template("{version").unwrap_err().contains("isn't closed"));
        assert!(plain("jdt-language-server-1.61.0-202609031315.tar.gz") && plain("v3.13.5"));
        for bad in ["", "../x", "a/b", ".hidden", "a b", "a;b"] {
            assert!(!plain(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_highest_version_is_taken() {
        let listing = r#"<a href="/jdtls/milestones/0.9.0/">0.9.0</a> <a href="/jdtls/milestones/1.9.0/">
            <a href="/jdtls/milestones/1.61.0/">1.61.0</a> <a href="/jdtls/milestones/1.10.0/">"#;
        assert_eq!(
            highest(listing, r"milestones/(\d+(?:\.\d+)+)", "x").unwrap(),
            "1.61.0",
            "numbers, not text"
        );
        let gh = r#"{"url": "…", "tag_name": "3.13.5", "name": "3.13.5"}"#;
        assert_eq!(highest(gh, r#""tag_name":\s*"([^"]+)""#, "x").unwrap(), "3.13.5");
        assert!(
            highest("nothing here", r"(\d+\.\d+)", "the page")
                .unwrap_err()
                .contains("nothing found at the page")
        );
        let evil = highest(r#""tag_name": "../../etc""#, r#""tag_name":\s*"([^"]+)""#, "x").unwrap_err();
        assert!(evil.contains("isn't a plain name"), "{evil}");
    }

    #[test]
    fn checksums_are_read_alone_or_by_name() {
        let h = "24bc7937de53dbb0e344e756e1bef60bcc12e19b93ea8383c93a86e41d593f1d";
        assert_eq!(checksum_of(&format!("{h}\n"), "a.tar.gz").as_deref(), Some(h));
        let sums = format!("{}  other.tar.gz\n{}  a.tar.gz\n", "0".repeat(64), h.to_uppercase());
        assert_eq!(checksum_of(&sums, "a.tar.gz").as_deref(), Some(h), "its own line, lowercased");
        assert_eq!(checksum_of(&format!("{h} *a.tar.gz"), "a.tar.gz").as_deref(), Some(h));
        assert_eq!(checksum_of(&sums, "b.tar.gz"), None, "not listed");
        assert_eq!(checksum_of("<html>not found</html>", "a.tar.gz"), None);
        assert_eq!(checksum_of("abc", "a.tar.gz"), None);
    }

    fn runtime() -> Runtime {
        Runtime {
            program: "java".into(),
            home: Some("JAVA_HOME".into()),
            ask: vec!["-version".into()],
            version: r#"version "(?:1\.)?(\d+)"#.into(),
            at_least: 21,
            get: "install a JDK 21".into(),
        }
    }

    #[test]
    fn what_it_runs_on_is_checked() {
        let rt = runtime();
        assert_eq!(major("openjdk version \"21.0.4\" 2024-07-16", &rt.version), Some(21));
        assert_eq!(major("java version \"1.8.0_392\"", &rt.version), Some(8));
        assert_eq!(major("openjdk version \"23-ea\"", &rt.version), Some(23));
        assert_eq!(major("no", &rt.version), None);
        let ok = ("/usr/bin/java".into(), Some(21));
        assert_eq!(runtime_blocker(&rt, Some(&ok), "jdtls"), None);
        assert_eq!(
            runtime_blocker(&rt, Some(&("/j".into(), None)), "jdtls"),
            None,
            "unread: not held against it"
        );
        let old =
            runtime_blocker(&rt, Some(&("/usr/lib/jvm/17/bin/java".into(), Some(17))), "jdtls").unwrap();
        assert_eq!(
            old,
            "jdtls runs on java 21 or newer — /usr/lib/jvm/17/bin/java is java 17: install a JDK 21"
        );
        let none = runtime_blocker(&rt, None, "jdtls").unwrap();
        assert!(none.contains("there's none here (JAVA_HOME, PATH)"), "{none}");
    }

    pub(crate) fn download(base: &str) -> Download {
        Download {
            url: format!("{base}/{{version}}/{{file}}"),
            version: Version::Newest(Pick { from: format!("{base}/"), pick: r"rel/(\d+(?:\.\d+)+)".into() }),
            file: Name::Pick(Pick {
                from: format!("{base}/{{version}}/latest.txt"),
                pick: r"(tool-\S+\.tar\.gz)".into(),
            }),
            sha256: Sum::Published(format!("{base}/{{version}}/{{file}}.sha256")),
            size: None,
            unpack: Unpack::TarGz,
            program: "bin/tool".into(),
            needs: vec!["python3".into()],
            runtime: Some(runtime()),
            rename: vec![],
        }
    }

    #[test]
    fn it_fits_where_its_tools_are() {
        let d = download("http://x");
        let has = |missing: &'static str| move |p: &str| p != missing && p != "wget";
        assert!(d.fits(has("nothing")));
        assert!(!d.fits(has("curl")), "no curl, no wget");
        assert!(!d.fits(has("tar")), "an archive needs tar");
        assert!(!d.fits(has("python3")), "what it needs");
        assert!(Download { unpack: Unpack::File, ..d.clone() }.fits(has("tar")), "a program needs no tar");
        assert_eq!(d.shown(), "a download from x (with curl or wget, tar, python3)");
        let named = Download {
            rename: vec![("x86_64".into(), "x64".into()), ("macos".into(), "darwin".into())],
            ..d
        };
        let v = Vars::new(&named);
        assert!(
            ["x64", "aarch64"].contains(&v.arch.as_str()) && ["darwin", "linux"].contains(&v.os.as_str())
        );
    }

    /// A local stand-in for a release site: answers `requests` requests from `files` (path → body), 404 for the
    /// rest. Its base URL.
    pub(crate) fn fake_site(files: Vec<(String, Vec<u8>)>, requests: usize) -> String {
        use std::io::{BufRead, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(requests) {
                let Ok(mut stream) = stream else { continue };
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                let mut header = String::new();
                while reader.read_line(&mut header).is_ok_and(|n| n > 2) {
                    header.clear();
                }
                let path = line.split_whitespace().nth(1).unwrap_or_default().to_string();
                let (status, body) = match files.iter().find(|(p, _)| *p == path) {
                    Some((_, b)) => ("200 OK", b.clone()),
                    None => ("404 Not Found", b"no".to_vec()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&body));
            }
        });
        format!("http://127.0.0.1:{port}/rel")
    }

    /// A stand-in archive: `bin/tool` (a marker, never run) and one more file.
    fn fake_archive(tmp: &Path) -> Vec<u8> {
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join("bin")).unwrap();
        std::fs::create_dir_all(src.join("lib")).unwrap();
        std::fs::write(src.join("bin/tool"), "# a stand-in, never run\n").unwrap();
        std::fs::write(src.join("lib/x.jar"), "PK").unwrap();
        let tgz = tmp.join("fake.tar.gz");
        let st = std::process::Command::new("tar")
            .arg("-czf")
            .arg(&tgz)
            .arg("-C")
            .arg(&src)
            .args(["bin", "lib"])
            .status()
            .unwrap();
        assert!(st.success());
        std::fs::read(&tgz).unwrap()
    }

    fn left(dir: &Path) -> Vec<std::ffi::OsString> {
        std::fs::read_dir(dir.parent().unwrap()).unwrap().flatten().map(|e| e.file_name()).collect()
    }

    #[test]
    fn a_release_is_found_checked_and_unpacked() {
        let tmp = std::env::temp_dir().join(format!("tarae-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let archive = fake_archive(&tmp);
        let sum = crate::sha256::hex(&archive);
        let name = "tool-1.61.0-202609031315.tar.gz";
        let listing =
            b"<a href=\"/rel/1.9.0/\"> <a href=\"/rel/1.61.0/\"> <a href=\"/rel/1.10.0/\">".to_vec();
        let site = |sum: Option<String>, latest: &str| {
            let mut files = vec![
                ("/rel/".to_string(), listing.clone()),
                ("/rel/1.61.0/latest.txt".to_string(), format!("{latest}\n").into_bytes()),
                (format!("/rel/1.61.0/{name}"), archive.clone()),
            ];
            files.extend(sum.map(|s| (format!("/rel/1.61.0/{name}.sha256"), s.into_bytes())));
            files
        };
        let dir = tmp.join("data/tool");
        std::fs::create_dir_all(dir.join("old")).unwrap();
        // Good: the highest release, checked, unpacked in place of what was there
        // (requests: the listing, latest.txt, the checksum, the archive)
        let base = fake_site(site(Some(format!("{sum}\n")), name), 4);
        let got = fetch(&download(&base), &dir).unwrap();
        assert_eq!(got, Got { version: "1.61.0".into(), program: dir.join("bin/tool") });
        assert!(dir.join("lib/x.jar").is_file() && !dir.join("old").exists());
        assert_eq!(installed(&dir), Some(dir.join("bin/tool")), "found there from then on");
        assert_eq!(left(&dir), ["tool"], "nothing half-done beside it");
        // A wrong checksum: nothing unpacked, what's there stays
        let base = fake_site(site(Some("0".repeat(64)), name), 4);
        let err = fetch(&download(&base), &dir).unwrap_err();
        assert!(err.contains("doesn't match its published checksum"), "{err}");
        assert_eq!(installed(&dir), Some(dir.join("bin/tool")), "the one before stays");
        assert_eq!(left(&dir), ["tool"]);
        // No checksum published: not downloaded
        let base = fake_site(site(None, name), 3);
        let err = fetch(&download(&base), &dir).unwrap_err();
        assert!(err.contains("no checksum published"), "{err}");
        // A bad version or name: not downloaded
        let bad = vec![("/rel/".to_string(), b"<a href=\"/rel/../x/\">".to_vec())];
        let err = fetch(&download(&fake_site(bad, 1)), &dir).unwrap_err();
        assert!(err.contains("nothing found"), "{err}");
        let base = fake_site(site(Some(sum.clone()), "tool-../../evil.tar.gz"), 2);
        assert!(fetch(&download(&base), &dir).is_err(), "latest.txt naming a path");
        // The program isn't where the pack says: nothing put in place
        let base = fake_site(site(Some(sum), name), 4);
        let wrong = Download { program: "bin/other".into(), ..download(&base) };
        assert!(fetch(&wrong, &dir).unwrap_err().contains("has no bin/other"));
        assert_eq!(installed(&dir), Some(dir.join("bin/tool")));
        assert_eq!(left(&dir), ["tool"]);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// The download is the program itself (a GitHub release's binary), named by a template.
    #[test]
    fn a_program_downloaded_as_it_is() {
        let tmp = std::env::temp_dir().join(format!("tarae-fetch-file-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let body = b"#!/bin/sh\n# a stand-in, never run\n".to_vec();
        let file = format!("tool_{}", std::env::consts::OS);
        let sums = format!("{}  other\n{}  {file}\n", "0".repeat(64), crate::sha256::hex(&body));
        let files = vec![
            ("/rel/latest".to_string(), br#"{"tag_name": "v0.4.1"}"#.to_vec()),
            ("/rel/v0.4.1/sha256sums.txt".to_string(), sums.into_bytes()),
            (format!("/rel/v0.4.1/{file}"), body.clone()),
        ];
        let base = fake_site(files, 3);
        let d = Download {
            url: format!("{base}/{{version}}/{{file}}"),
            version: Version::Newest(Pick {
                from: format!("{base}/latest"),
                pick: r#""tag_name":\s*"([^"]+)""#.into(),
            }),
            file: Name::Is("tool_{os}".into()),
            sha256: Sum::Published(format!("{base}/{{version}}/sha256sums.txt")),
            size: None,
            unpack: Unpack::File,
            program: "tool".into(),
            needs: vec![],
            runtime: None,
            rename: vec![],
        };
        let dir = tmp.join("tool");
        std::fs::create_dir_all(&tmp).unwrap();
        let got = fetch(&d, &dir).unwrap();
        assert_eq!(got.version, "v0.4.1");
        assert_eq!(std::fs::read(&got.program).unwrap(), body);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&got.program).unwrap().permissions().mode() & 0o111, 0o111, "runnable");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A release the pack pins — its version, SHA-256 and size (java-debug's): nothing looked up, only the
    /// file asked for, and what doesn't match all three isn't put in place.
    #[test]
    fn a_pinned_release_is_checked_against_the_pack() {
        let tmp = std::env::temp_dir().join(format!("tarae-fetch-pinned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let body = b"PK a stand-in jar, never run".to_vec();
        let name = "plugin-0.53.1.jar".to_string();
        let pinned = |base: &str, sum: String, size: usize| Download {
            url: format!("{base}/{{version}}/{{file}}"),
            version: Version::Pinned("0.53.1".into()),
            file: Name::Is("plugin-{version}.jar".into()),
            sha256: Sum::Pinned(sum),
            size: Some(size as u64),
            unpack: Unpack::File,
            program: "{file}".into(),
            needs: vec![],
            runtime: None,
            rename: vec![],
        };
        let site = || fake_site(vec![(format!("/rel/0.53.1/{name}"), body.clone())], 1);
        let dir = tmp.join("plugin");
        let got =
            fetch(&pinned(&site(), crate::sha256::hex(&body).to_uppercase(), body.len()), &dir).unwrap();
        assert_eq!(got, Got { version: "0.53.1".into(), program: dir.join(&name) });
        assert_eq!(installed(&dir), Some(dir.join(&name)));
        let err = fetch(&pinned(&site(), "0".repeat(64), body.len()), &dir).unwrap_err();
        assert!(err.contains("doesn't match the checksum its pack pins"), "{err}");
        let err = fetch(&pinned(&site(), crate::sha256::hex(&body), 2960556), &dir).unwrap_err();
        assert!(err.contains("not the 2960556 its pack says"), "{err}");
        assert_eq!(installed(&dir), Some(dir.join(&name)), "the one before stays");
        let d = pinned("https://repo1.maven.org/x", "0".repeat(64), 1);
        assert_eq!(
            d.said(),
            "version 0.53.1 from repo1.maven.org — checked against the SHA-256 its pack pins"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
