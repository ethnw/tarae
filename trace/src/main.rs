//! `tarae-trace <probe>` — a tarae measured: `offcpu` (where its threads wait, by stack), `syscalls` (its
//! syscall mix), `pty` (its writes to the terminal, entry to return). Runs privileged (root, or CAP_BPF +
//! CAP_PERFMON; CAP_SYSLOG to name kernel frames); the tarae it watches stays as it is.
//!
//! ```text
//! sudo tarae-trace offcpu --pid 4242                    # a tarae already running, until it exits/Ctrl-C
//! sudo tarae-trace offcpu --pid 4242 --children --for 1m   # + what it starts: Claude's runs, servers, git
//! sudo tarae-trace syscalls --comm tarae                # every tarae, whichever starts
//! sudo tarae-trace pty -- tarae src/main.rs             # one started here, until it exits
//! sudo tarae-trace offcpu --comm tarae -- cargo test …  # while a command runs (what `cargo xtask trace` does)
//! ```
//!
//! It ends when every `--pid` has exited or the command has — and never later than `--for` (`DEFAULT_FOR`
//! when not given: a target that never exits mustn't keep it waiting); Ctrl-C, SIGTERM or SIGHUP end it
//! sooner. Each way it then writes what it has, `DIR/<probe>-NAME.json` (and `.folded` for a probe with
//! stacks; given back to the user under sudo), and says the top of it on stderr. Exits with the command's
//! code, else 0. A command still running when it ends is left running.

use std::path::PathBuf;

const USAGE: &str = "usage: tarae-trace offcpu|syscalls|pty [--pid P]… [--children] [--comm C] \
                     [--for 20s|2m|1h] [--name N] [--out DIR] [--stacks SYSCALL,… (syscalls)] \
                     [-- command [args …]]";
const PROBES: [&str; 3] = ["offcpu", "syscalls", "pty"];

/// The longest a trace runs when `--for` isn't given, in seconds.
const DEFAULT_FOR: f64 = 600.0;

#[derive(Debug, Default, PartialEq)]
struct Args {
    probe: String,
    pids: Vec<u32>,
    children: bool,
    comm: Option<String>,
    /// The most it runs, in seconds — `--for`, else `DEFAULT_FOR`.
    secs: f64,
    /// `--for` was given.
    secs_given: bool,
    name: Option<String>,
    out: PathBuf,
    /// The syscalls whose callers' stacks are kept (`--stacks`, the syscalls probe's).
    stacks: Vec<String>,
    cmd: Vec<String>,
}

/// `20`, `20s`, `500ms`, `2m`, `1h`, `1.5s` → seconds (more than none).
fn duration(s: &str) -> Option<f64> {
    let (n, unit) = match s.find(|c: char| c.is_ascii_alphabetic()) {
        Some(i) => s.split_at(i),
        None => (s, "s"),
    };
    let scale = match unit {
        "ms" => 0.001,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        _ => return None,
    };
    let v: f64 = n.parse().ok()?;
    (v > 0.0 && v.is_finite()).then_some(v * scale)
}

fn parse(args: Vec<String>) -> Result<Args, String> {
    let mut it = args.into_iter();
    let probe = it.next().filter(|p| PROBES.contains(&p.as_str())).ok_or(USAGE)?;
    let mut a = Args { probe, out: ".".into(), secs: DEFAULT_FOR, ..Args::default() };
    let bad = |what: &str, v: &str| format!("{what}: {v:?}\n{USAGE}");
    while let Some(arg) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{arg} takes a value\n{USAGE}"));
        match arg.as_str() {
            "--pid" => {
                let v = value()?;
                a.pids.push(v.parse().map_err(|_| bad("not a pid", &v))?);
            }
            "--children" => a.children = true,
            "--comm" => a.comm = Some(value()?),
            "--for" => {
                let v = value()?;
                a.secs = duration(&v).ok_or_else(|| bad("not a duration (20s, 2m, 1h)", &v))?;
                a.secs_given = true;
            }
            "--name" => a.name = Some(value()?),
            "--stacks" => a.stacks.extend(value()?.split(',').filter(|s| !s.is_empty()).map(String::from)),
            "--out" => a.out = value()?.into(),
            "--" => a.cmd.extend(it.by_ref()),
            _ => return Err(bad("unknown", &arg)),
        }
    }
    if a.pids.is_empty() && a.comm.is_none() && a.cmd.is_empty() {
        return Err(format!("nothing to trace: --pid, --comm or a command\n{USAGE}"));
    }
    Ok(a)
}

impl Args {
    /// The output's name: given, else what's traced.
    fn name(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        if let Some(c) = &self.comm {
            return c.clone();
        }
        if let Some(p) = self.pids.first() {
            return format!("pid{p}");
        }
        let program = self.cmd.first().map(String::as_str).unwrap_or("run");
        program.rsplit('/').next().unwrap_or(program).to_string()
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    if let Err(e) = parse(std::env::args().skip(1).collect()) {
        eprintln!("{e}");
    }
    eprintln!("tarae-trace: Linux only (eBPF) — `cargo xtask trace` runs it in a container");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(match parse(std::env::args().skip(1).collect()).and_then(run) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("tarae-trace: {e}");
            2
        }
    });
}

/// The signal that asked it to stop (0: none yet).
#[cfg(target_os = "linux")]
static STOP: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(target_os = "linux")]
extern "C" fn on_signal(sig: libc::c_int) {
    STOP.store(sig, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(target_os = "linux")]
fn run(a: Args) -> Result<i32, String> {
    use tarae_trace::{offcpu, ptywrite, syscalls};
    match a.probe.as_str() {
        "offcpu" => trace::<offcpu::Tracer>(a),
        "syscalls" => trace::<syscalls::Tracer>(a),
        _ => trace::<ptywrite::Tracer>(a),
    }
}

#[cfg(target_os = "linux")]
fn trace<P: tarae_trace::attach::Probe>(a: Args) -> Result<i32, String> {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    use tarae_trace::Output;
    use tarae_trace::attach::Target;
    use tarae_trace::procs;

    let handler = on_signal as extern "C" fn(libc::c_int);
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::signal(sig, handler as libc::sighandler_t);
        }
    }
    let target = Target { pids: a.pids.clone(), comm: a.comm.clone(), children: a.children };
    // (a command alone is what's traced; with --pid/--comm it's only how long)
    let only_cmd = a.pids.is_empty() && a.comm.is_none();
    // (a command alone: nothing yet — its pid is added once it's started)
    let mut t = P::attach(&target).map_err(|e| e.to_string())?;
    t.keep_stacks_for(&a.stacks).map_err(|e| e.to_string())?;
    let mut child = match a.cmd.split_first() {
        Some((program, rest)) => {
            let c = std::process::Command::new(program)
                .args(rest)
                .spawn()
                .map_err(|e| format!("{program}: {e}"))?;
            if only_cmd {
                t.add(c.id()).map_err(|e| e.to_string())?;
            }
            Some(c)
        }
        None => None,
    };
    let what = match (&child, a.pids.as_slice()) {
        (Some(_), _) => "until the command exits, ".to_string(),
        (None, []) => String::new(),
        (None, p) => {
            format!("until pid {} exits, ", p.iter().map(u32::to_string).collect::<Vec<_>>().join(", "))
        }
    };
    let bound = if a.secs_given { String::new() } else { " (--for to change)".into() };
    eprintln!(
        "tarae-trace: tracing {} of {}{} — {what}at most {}{bound}; Ctrl-C ends it sooner",
        P::NAME,
        a.name(),
        if a.children { " and its children" } else { "" },
        say_secs(a.secs),
    );
    let deadline = Instant::now() + Duration::from_secs_f64(a.secs);
    let mut status = None;
    let mut ended = String::new();
    t.run_until(|| {
        let sig = STOP.load(Ordering::Relaxed);
        ended = if sig != 0 {
            format!("signal {sig}")
        } else if Instant::now() >= deadline {
            format!("{} are up", say_secs(a.secs))
        } else if let Some(c) = child.as_mut() {
            status = c.try_wait().ok().flatten();
            if status.is_none() {
                return false;
            }
            "the command exited".into()
        } else if !a.pids.is_empty() && !a.pids.iter().any(|&p| procs::alive(p)) {
            "the traced pid exited".into()
        } else {
            return false;
        };
        true
    });
    eprintln!("tarae-trace: ended — {ended}");
    let name = a.name();
    let r = t.finish(&name).map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&a.out).map_err(|e| format!("{}: {e}", a.out.display()))?;
    let json = a.out.join(format!("{}-{name}.json", P::NAME));
    let text = serde_json::to_string_pretty(&r.to_json()).unwrap();
    std::fs::write(&json, text + "\n").map_err(|e| format!("{}: {e}", json.display()))?;
    let mut written = vec![json];
    if let Some(stacks) = r.folded() {
        let folded = a.out.join(format!("{}-{name}.folded", P::NAME));
        std::fs::write(&folded, stacks).map_err(|e| format!("{}: {e}", folded.display()))?;
        written.push(folded);
    }
    give_back(&written);
    eprint!("{}", r.summary(15));
    let files: Vec<String> = written.iter().map(|f| f.display().to_string()).collect();
    eprintln!("→ {}", files.join(" · "));
    // (a command still running after Ctrl-C or --for is the user's to stop — it got the Ctrl-C too)
    let status = status.or_else(|| child.as_mut().and_then(|c| c.try_wait().ok().flatten()));
    Ok(status.and_then(|s| s.code()).unwrap_or(0))
}

/// `20 s`, `10 min`, `1.5 h`.
#[cfg(any(target_os = "linux", test))]
fn say_secs(s: f64) -> String {
    let n = |v: f64| if v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v:.1}") };
    if s >= 3600.0 {
        format!("{} h", n(s / 3600.0))
    } else if s >= 60.0 {
        format!("{} min", n(s / 60.0))
    } else {
        format!("{} s", n(s))
    }
}

/// Under sudo, the files written go to the user who ran it — not root's in their folder.
#[cfg(target_os = "linux")]
fn give_back(files: &[PathBuf]) {
    let id = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok());
    if let (Some(uid), Some(gid)) = (id("SUDO_UID"), id("SUDO_GID")) {
        for f in files {
            let _ = std::os::unix::fs::chown(f, Some(uid), Some(gid));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Args, String> {
        parse(s.split_whitespace().map(String::from).collect())
    }

    /// The shapes: a running tarae by pid (with its children), every tarae by comm, a command alone, a comm
    /// while a command runs; something to trace is required, values are checked.
    #[test]
    fn what_to_trace_and_for_how_long() {
        let a = p("offcpu --pid 4242 --children --for 60").unwrap();
        assert_eq!(
            (a.probe.as_str(), a.pids.as_slice(), a.children, a.secs, a.name()),
            ("offcpu", &[4242][..], true, 60.0, "pid4242".into())
        );
        // Never unbounded: no --for is DEFAULT_FOR
        let a = p("offcpu --pid 4242").unwrap();
        assert_eq!((a.secs, a.secs_given, say_secs(a.secs)), (DEFAULT_FOR, false, "10 min".into()));
        let a = p("offcpu --comm tarae -- cargo test --test perf -- --exact x").unwrap();
        assert_eq!((a.comm.as_deref(), a.cmd.len(), a.name()), (Some("tarae"), 7, "tarae".into()));
        assert_eq!(a.cmd[4..], ["--", "--exact", "x"], "the command's own -- stays its own");
        assert_eq!(p("offcpu -- /usr/local/bin/tarae a.rs").unwrap().name(), "tarae");
        assert_eq!(p("offcpu --pid 1 --pid 2 --name n --out /tmp/o").unwrap().out, PathBuf::from("/tmp/o"));
        assert!(p("offcpu").unwrap_err().starts_with("nothing to trace"));
        assert!(p("offcpu --pid x").unwrap_err().starts_with("not a pid"));
        for bad in ["-3", "0", "3x", "s", "1e400"] {
            assert!(
                p(&format!("offcpu --pid 1 --for {bad}")).unwrap_err().starts_with("not a duration"),
                "{bad}"
            );
        }
        let d = |s| duration(s).unwrap();
        assert_eq!(
            (d("20"), d("20s"), d("500ms"), d("2m"), d("1h"), d("1.5s")),
            (20.0, 20.0, 0.5, 120.0, 3600.0, 1.5)
        );
        assert_eq!(
            (say_secs(20.0), say_secs(90.0), say_secs(5400.0)),
            ("20 s".into(), "1.5 min".into(), "1.5 h".into())
        );
        assert!(p("offcpu --pid").unwrap_err().starts_with("--pid takes a value"));
        assert!(p("offcpu --bogus").unwrap_err().starts_with("unknown"));
        // Each probe takes the same flags
        for probe in PROBES {
            assert_eq!(p(&format!("{probe} --comm tarae")).unwrap().probe, probe);
        }
        assert!(p("tracepoints --pid 1").unwrap_err().starts_with("usage"));
        let a = p("syscalls --comm tarae --stacks mprotect,munmap --stacks brk").unwrap();
        assert_eq!(a.stacks, ["mprotect", "munmap", "brk"]);
    }
}
