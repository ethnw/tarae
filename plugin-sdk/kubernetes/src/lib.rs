//! Kubernetes in the code: open a manifest and each object in it has its state beside its `kind:` line —
//! `2/3 ready · 1 CrashLoopBackOff`, `Ready`, `not in prod-eks` — live. The status line says which cluster
//! it is (the kubeconfig's current context); the side panel's `k8s` tab is the file in front's (as the
//! outline is): its objects, and the pods of those that aren't fine — and, below, the other open files that
//! aren't fine. From there, or from a line (`:kubernetes-here`): the
//! object's logs, events, live YAML, or where it's defined. `:kubernetes` turns it on (the first time, the
//! tab opens) and, once it's on, takes you to the tab; `:kubernetes-off` turns it off. Browsing a cluster
//! is kubectl's, in the terminal.
//!
//! All of it is Rust in WebAssembly: the kubeconfig read and understood here, TLS (rustls + RustCrypto,
//! with the client certificate or token), HTTP, and the API — requests from kube-core, types from
//! k8s-openapi. tarae only carries bytes, and only because you allowed it (`permissions()`). The one
//! program it runs is EKS's credential one, `aws eks get-token`, when the kubeconfig says so.
//!
//! What it watches is one kind in one namespace at a time (the kinds its open manifests hold) — only the
//! object named, when the manifests have one of the kind there (the server sends nothing else), and a
//! namespace's pods by the selectors of its workloads: what the plugin holds and works on is what's open,
//! not how big the cluster is. A list first (500 objects a page), then a watch from its `resourceVersion`;
//! a change to an object nobody has open isn't worked out again. A watch that ends goes on from
//! there; one whose version is too old (410) lists afresh; failures are tried again after a pause that
//! grows. A kind's name for the API (its plural, whether it's namespaced) comes from the cluster, so any
//! kind works — a Flux HelmRelease says whether it's Ready. Secrets are watched without their values.
//!
//! A file Kustomize or Helm makes objects of is read as they make them: a base through the build of each
//! root (`kustomize build`) that takes it in, a chart's template through `helm template` for each release
//! of the chart the cluster has (found by the labels Helm puts on what it installs — whoever installed it).
wit_bindgen::generate!({ path: "../wit", world: "plugin", additional_derives: [PartialEq] });

mod cause;
mod fields;
mod helm;
mod kubeconfig;
mod kustomize;
mod manifest;
mod refs;
mod status;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use kube_core::discovery::ApiResource;
use kube_core::params::{GetParams, ListParams, WatchParams};
use kube_core::{DynamicObject, Resource, WatchEvent};
use serde_json::Value;
use tarae::plugin::types::{Connect, Hint, Hints, Pick, Side, SideRow, Spawn, Tone, View};
use tarae_plugin::http::{Lines, Response};
use tarae_plugin::tls::{self, Tls};

use kubeconfig::{Exec, Loading};
use kustomize::Built;
use manifest::{Doc, Via};

// ── Ids ────────────────────────────────────────────────────────────────────

/// The kubeconfig and its files are read with these; connections count up from `FIRST_CONN`; a stream's
/// retry timer is `RETRY + its id`; the buffers it shows count up from `FIRST_VIEW`.
const ENV: u64 = 1;
const KUBECONFIG: u64 = 2;
const FILES: u64 = 10;
const EXEC: u64 = 40;
const REFRESH: u64 = 52;
const PICK_ACTION: u64 = 61;
/// Kustomize: finding the project's kustomizations, learning where its own folder is, offering kustomize;
/// reading kustomization `n`, writing root `n`'s wrapper, building root `n`.
const FIND_KZ: u64 = 80;
const OWN_DIR: u64 = 81;
const KZ_INSTALL: u64 = 83;
const KZ_READ: u64 = 1 << 36;
const KZ_WRITE: u64 = 1 << 37;
const KZ_BUILD: u64 = 1 << 38;
/// Helm: finding the project's charts, offering helm; reading `Chart.yaml` `n`, rendering release `n`.
const FIND_CHARTS: u64 = 85;
const HELM_INSTALL: u64 = 86;
const HELM_READ: u64 = 1 << 35;
/// Helm values: finding the project's chart templates; reading template `n` (of a chart whose values are open).
const FIND_TEMPLATES: u64 = 87;
const TPL_READ: u64 = 1 << 34;
/// A release's own values: `helm get values` for release `n`, and the file they're written to for its render.
const VALUES_GET: u64 = 1 << 33;
const VALUES_WRITE: u64 = (1 << 33) | (1 << 31);
const HELM_RENDER: u64 = 1 << 39;
const FIRST_CONN: u64 = 1000;
const RETRY: u64 = 1 << 32;
const FIRST_VIEW: u64 = 1 << 40;

/// At most this many kinds-in-a-namespace are watched at once (tarae allows a plugin 16 connections; the
/// rest are for logs and the like).
const MAX_STREAMS: usize = 12;
/// Workloads (their pods are looked at too) and what else wants the pods of its namespace.
const WITH_PODS: [&str; 6] = ["Deployment", "StatefulSet", "DaemonSet", "ReplicaSet", "Job", "Service"];
/// What's looked at for the labels Helm puts on what it installs (a chart's releases): the kinds charts
/// nearly all have one of — (apiVersion, plural).
const LABELED: [(&str, &str); 6] = [
    ("apps/v1", "deployments"),
    ("apps/v1", "statefulsets"),
    ("apps/v1", "daemonsets"),
    ("v1", "services"),
    ("v1", "configmaps"),
    ("v1", "serviceaccounts"),
];

// ── The cluster ────────────────────────────────────────────────────────────

/// What connecting takes.
struct Cluster {
    context: String,
    host: String,
    port: u64,
    /// The server URL's path (Rancher's `/k8s/clusters/…`), put before every request's.
    base: String,
    /// The name its certificate has to carry.
    server_name: String,
    tls: Arc<tls::Config>,
    token: Option<String>,
}

/// One kind in one namespace (none: cluster-wide), as the API names it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Watched {
    api_version: String,
    plural: String,
    ns: Option<String>,
}

impl Watched {
    fn resource(&self, kind: &str) -> ApiResource {
        let (group, version) = self.api_version.split_once('/').unwrap_or(("", &self.api_version));
        ApiResource {
            group: group.into(),
            version: version.into(),
            api_version: self.api_version.clone(),
            kind: kind.into(),
            plural: self.plural.clone(),
        }
    }

    fn url(&self, kind: &str) -> String {
        DynamicObject::url_path(&self.resource(kind), self.ns.as_deref())
    }
}

/// Which of a kind's objects in a namespace a stream asks for: all of them, the one a manifest names
/// (the server sends nothing else), or the pods a workload selects (`app=web`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Select {
    All,
    Name(String),
    Labels(String),
}

impl Select {
    /// Whether the object named `name` is among what it asks for (a label selector's: not known by name).
    fn covers(&self, name: &str) -> bool {
        match self {
            Select::All => true,
            Select::Name(n) => n == name,
            Select::Labels(_) => false,
        }
    }
}

/// A kind in a namespace, listed and then watched.
struct Stream {
    what: Watched,
    select: Select,
    /// The names the manifests have among what it watches (a change to another is nobody's; none: any
    /// change may be someone's — a workload's pods).
    names: BTreeSet<String>,
    kind: String,
    /// Only their metadata comes (a Secret's values are never fetched).
    metadata_only: bool,
    objects: BTreeMap<String, DynamicObject>,
    /// A list coming in page by page: it replaces `objects` once it's all in.
    listing: Option<BTreeMap<String, DynamicObject>>,
    loaded: bool,
    version: String,
    watch: Option<u64>,
    /// When the watch started (one that ends at once is a failure), and whether the server said its
    /// version is too old (410: a fresh list, not a watch from there).
    watch_started: u64,
    relist: bool,
    /// Failures in a row (the pause before the next try grows with them), and the last one.
    retry: u32,
    error: Option<String>,
}

/// What a group/version of the API serves: (kind, plural, namespaced) — asked of the cluster once.
enum Served {
    Asking,
    Kinds(Vec<(String, String, bool)>),
    Failed(String),
}

/// A root's build, or a release's render: running (its output so far), done (what it made), or failed (why).
enum Build<T = Built> {
    Running(Vec<String>),
    Done(Vec<T>),
    Failed(String),
}

/// What a release was given (its values beyond the chart's): being asked, written to a file to render with,
/// rendered with (the file emptied: they may hold its secrets — written again to render again), none (`helm
/// get values` said `null`), or not known (why — read access to the release's Secret, say).
#[derive(Clone, Debug, PartialEq)]
enum Given {
    Asking(Vec<String>),
    Writing(Value),
    Got(Value, String),
    Spent(Value),
    Nothing,
    Unknown(String),
}

/// A manifest open in tarae: its objects, as of its `version`.
struct File {
    version: u64,
    docs: Vec<Doc>,
}

// ── Requests ───────────────────────────────────────────────────────────────

/// What a connection is for.
enum Purpose {
    Discover(String),
    List(u64),
    Watch(u64),
    /// (the view's key, its title, whether to mask values — a Secret's)
    Yaml(String, String, bool),
    /// (the view's key, its title, the names whose events it shows — an object's, and its pods' and replica
    /// sets', theirs starting with its name — and what goes above them: a pod described)
    Events(String, String, String, String),
    Logs(u64),
    /// The objects Helm labeled, of `LABELED[i]` — cluster-wide, or (not allowed that) the context's
    /// namespace — for the look numbered so (a later one makes it stale).
    Labeled(u64, usize, bool),
}

struct Req {
    purpose: Purpose,
    /// The HTTP request, sent once TLS is up.
    http: Vec<u8>,
    tls: Option<Tls>,
    resp: Response,
    lines: Lines,
    /// The body so far (a log's: bytes that don't end a character yet).
    body: Vec<u8>,
}

/// How an object in a manifest is doing.
struct Seen {
    text: String,
    tone: Tone,
    /// What the side tab says of it (`3/3 ↻2`, `3 pods`).
    brief: String,
}

#[derive(Default)]
struct State {
    on: bool,
    /// Where the kubeconfig is (its relative paths are from its folder).
    kubeconfig: String,
    loading: Option<Loading>,
    cluster: Option<Cluster>,
    /// The context's namespace, for manifests that don't say theirs.
    namespace: String,
    /// What keeps it from showing anything (the kubeconfig, signing in).
    error: Option<String>,
    exec: Option<Exec>,
    exec_out: Vec<String>,
    exec_running: bool,
    reqs: HashMap<u64, Req>,
    next: u64,
    served: HashMap<String, Served>,
    streams: BTreeMap<u64, Stream>,
    stream_ids: u64,
    files: BTreeMap<String, File>,
    /// What was sent: each file's hints (and the version of it they were for), the side tab, the badge —
    /// and whether the side panel was opened at its tab yet (the first time it's on).
    sent: HashMap<String, (u64, Vec<Hint>)>,
    side: Option<Side>,
    badge: Option<(String, Tone)>,
    side_opened: bool,
    /// The object a pick of what to do is about: (path, its document's index).
    picking: Option<(String, usize)>,
    /// The buffers it showed, by what they show.
    views: HashMap<String, u64>,
    view_ids: u64,
    /// Kustomize: the project's kustomizations, the ones being read (and how many are left), its own
    /// folder, the roots it built (numbered, for ids), and why it can't build (kustomize missing).
    tree: kustomize::Tree,
    reading: (Vec<String>, kustomize::Tree, usize),
    own_dir: Option<String>,
    roots: Vec<String>,
    builds: BTreeMap<String, Build>,
    no_kustomize: Option<String>,
    /// Helm: the project's charts (folder → name; `None`: not looked for yet) and the ones being read;
    /// what the cluster's objects' Helm labels say — (chart label, release, namespace) — once looked at,
    /// and the look going on; each release's render (numbered, for ids); why it can't render (helm
    /// missing); the charts' `values.yaml` open (and their versions).
    charts: Option<BTreeMap<String, String>>,
    chart_reading: (Vec<String>, BTreeMap<String, String>, usize),
    labeled: Option<Vec<(String, String, String)>>,
    labeling: (u64, Vec<(String, String, String)>),
    renders: BTreeMap<(String, helm::Release), Build<helm::Rendered>>,
    render_keys: Vec<(String, helm::Release)>,
    no_helm: Option<String>,
    values: BTreeMap<String, (u64, Vec<fields::Field>)>,
    /// The project's chart templates, and those read (for a chart whose `values.yaml` is open: where each value
    /// goes) — and being read.
    templates: Vec<String>,
    chart_files: BTreeMap<String, Vec<Doc>>,
    tpl_reading: Vec<String>,
    /// Each release's own values (`helm get values`): what it was given, as a file to render it with.
    release_values: BTreeMap<(String, helm::Release), Given>,
    /// Whether what's shown may have changed since it was last worked out (a watch's change to an object
    /// nobody has open doesn't), and whether what's watched may have to change (a workload came: its pods).
    dirty: bool,
    refollow: bool,
    /// The file in front (`shown`): the side tab is its objects.
    front: Option<String>,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}

fn api_error(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| String::from_utf8_lossy(&body[..body.len().min(200)]).into_owned())
}

fn name_of(o: &DynamicObject) -> String {
    o.metadata.name.clone().unwrap_or_default()
}

/// A group's detail and tone from its members': how many errors and warnings, else `fine` as given.
fn tally(tones: &[Tone], fine: String) -> (String, Tone) {
    let count = |t: Tone| tones.iter().filter(|x| **x == t).count();
    let detail = match (count(Tone::Error), count(Tone::Warning)) {
        (0, 0) => fine,
        (e, 0) => format!("✗{e}"),
        (0, w) => format!("▲{w}"),
        (e, w) => format!("✗{e} ▲{w}"),
    };
    let worst = tones.iter().copied().min_by_key(|t| rank(*t)).unwrap_or(Tone::Info);
    (detail, worst)
}

/// `text` in lines of about `width` characters, broken between words.
fn wrapped(text: &str, width: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match out.last_mut() {
            Some(l) if l.chars().count() + 1 + word.chars().count() <= width => {
                l.push(' ');
                l.push_str(word);
            }
            _ => out.push(word.to_string()),
        }
    }
    out
}

/// One hint a line (tarae shows the first): what's said of it, in order, the worst tone.
fn together(hints: Vec<Hint>) -> Vec<Hint> {
    let mut out: Vec<Hint> = Vec::new();
    for h in hints {
        match out.iter_mut().find(|o| o.line == h.line) {
            Some(o) => {
                o.text = format!("{}  ·  {}", o.text, h.text);
                if rank(h.tone) < rank(o.tone) {
                    o.tone = h.tone;
                }
            }
            None => out.push(h),
        }
    }
    out
}

fn rank(t: Tone) -> u8 {
    match t {
        Tone::Error => 0,
        Tone::Warning => 1,
        Tone::Info => 2,
        Tone::Ok => 3,
    }
}

impl State {
    fn context(&self) -> &str {
        self.cluster.as_ref().map_or("…", |c| c.context.as_str())
    }

    // ── Turning it on and off ──

    /// On: from the kubeconfig (what's open is looked at once connected).
    fn start(&mut self) -> Vec<Action> {
        let mut out = self.stop_all();
        self.on = true;
        self.error = None;
        self.cluster = None;
        self.loading = None;
        self.exec = None;
        self.served.clear();
        out.push(Action::ReadEnv((ENV, "KUBECONFIG".into())));
        out
    }

    /// Off: every connection closed, the hints, the side tab and the badge gone.
    fn stop(&mut self) -> Vec<Action> {
        let mut out = self.stop_all();
        self.on = false;
        for (path, (version, hints)) in std::mem::take(&mut self.sent) {
            if !hints.is_empty() {
                out.push(Action::Hints(Hints { path, version, hints: Vec::new() }));
            }
        }
        (self.side, self.badge) = (None, None);
        out.extend([Action::Side(None), Action::Badge(None)]);
        out
    }

    fn stop_all(&mut self) -> Vec<Action> {
        let mut out: Vec<Action> = self.reqs.drain().map(|(id, _)| Action::Stop(id)).collect();
        out.extend(self.streams.keys().map(|sid| Action::Stop(RETRY + sid)));
        self.streams.clear();
        self.exec_running = false;
        out.extend([Action::Stop(REFRESH), Action::Stop(EXEC)]);
        out
    }

    // ── The kubeconfig, signing in ──

    fn read_kubeconfig(&mut self, text: &[u8]) -> Vec<Action> {
        let plan = match kubeconfig::plan(text, &self.kubeconfig) {
            Ok(p) => p,
            Err(e) => return self.fail(e),
        };
        let mut l = plan.loading;
        let mut out = Vec::new();
        for (what, path) in plan.files {
            let id = FILES + l.files.len() as u64;
            l.files.insert(id, what);
            out.push(Action::ReadFile((id, path)));
        }
        self.exec = plan.exec;
        self.loading = Some(l);
        out.extend(self.run_exec());
        if out.is_empty() {
            return self.connect_cluster();
        }
        out
    }

    fn file_read(&mut self, id: u64, bytes: Vec<u8>) -> Vec<Action> {
        let Some(l) = &mut self.loading else { return Vec::new() };
        match l.files.remove(&id) {
            Some("ca") => l.ca = Some(bytes),
            Some("cert") => l.cert = Some(bytes),
            Some("token") => l.token = Some(String::from_utf8_lossy(&bytes).trim().to_string()),
            Some(_) => l.key = Some(bytes),
            None => return Vec::new(),
        }
        if l.files.is_empty() && !l.exec_pending { self.connect_cluster() } else { Vec::new() }
    }

    /// Runs the credential program (unless it's running): its token comes back as it ends.
    fn run_exec(&mut self) -> Vec<Action> {
        let Some(e) = &self.exec else { return Vec::new() };
        if self.exec_running {
            return Vec::new();
        }
        self.exec_running = true;
        self.exec_out.clear();
        let env = e.env.iter().flatten().map(|v| (v.name.clone(), v.value.clone())).collect();
        vec![Action::Spawn(Spawn { id: EXEC, program: e.command.clone(), args: e.args.clone().unwrap_or_default(), env })]
    }

    /// The credential program ended: its token (an `ExecCredential`) — then connecting, or a fresher token
    /// for what comes next — and when to run it again.
    fn exec_done(&mut self, code: Option<u64>, err: &[String]) -> Vec<Action> {
        self.exec_running = false;
        let program = self.exec.as_ref().map_or("", |e| e.command.as_str()).to_string();
        let said = err.iter().rev().find(|l| !l.trim().is_empty()).map_or("", |l| l.trim());
        let got = match code {
            Some(0) => serde_json::from_str::<Value>(&self.exec_out.join("\n"))
                .map_err(|e| format!("{program}'s credential: {e}"))
                .and_then(|v| {
                    let st = &v["status"];
                    let token = st["token"].as_str().map(str::to_string);
                    let cert = st["clientCertificateData"].as_str().map(|c| c.as_bytes().to_vec());
                    let key = st["clientKeyData"].as_str().map(|k| k.as_bytes().to_vec());
                    if token.is_none() && cert.is_none() {
                        return Err(format!("{program} gave no token"));
                    }
                    Ok((token, cert, key, status::millis(&st["expirationTimestamp"])))
                }),
            _ => {
                let hint = if said.to_lowercase().contains("sso") { " — aws sso login?" } else { "" };
                Err(format!("{program} failed: {said}{hint}"))
            }
        };
        let (token, cert, key, expires) = match got {
            Ok(g) => g,
            Err(why) => {
                // Starting: nothing to show. Later: said, and tried again in a minute
                if self.loading.take().is_some() {
                    return self.fail(why);
                }
                let mut out = self.fail(why);
                out.push(Action::Timer((REFRESH, 60_000)));
                return out;
            }
        };
        let mut out = Vec::new();
        // Again a minute before it expires (not sooner than 10 s from now)
        if let Some(t) = expires {
            let left = (t - tarae_plugin::now::millis() as i64 - 60_000).clamp(10_000, 86_400_000);
            out.push(Action::Timer((REFRESH, left as u64)));
        }
        if let Some(l) = &mut self.loading {
            l.token = token.or(l.token.take());
            if cert.is_some() {
                (l.cert, l.key) = (cert, key);
            }
            l.exec_pending = false;
            if l.files.is_empty() {
                out.extend(self.connect_cluster());
            }
        } else if let (Some(c), Some(t)) = (&mut self.cluster, token) {
            c.token = Some(t);
            self.error = None;
        }
        out
    }

    fn connect_cluster(&mut self) -> Vec<Action> {
        let Some(l) = self.loading.take() else { return Vec::new() };
        let (host, port, base) = match kubeconfig::server(&l.server) {
            Ok(s) => s,
            Err(e) => return self.fail(e),
        };
        let Some(ca) = &l.ca else { return self.fail("the kubeconfig names no CA for its cluster".into()) };
        let client = match (&l.cert, &l.key) {
            (Some(c), Some(k)) => Some((c.as_slice(), k.as_slice())),
            _ => None,
        };
        let config = match tls::config(ca, client) {
            Ok(c) => c,
            Err(e) => return self.fail(e),
        };
        let server_name = l.server_name.clone().unwrap_or_else(|| host.clone());
        self.cluster = Some(Cluster { context: l.context, host, port, base, server_name, tls: config, token: l.token });
        self.namespace = l.namespace.unwrap_or_else(|| "default".into());
        self.error = None;
        let mut out = self.scan();
        out.extend(self.follow());
        out
    }

    /// Nothing can be shown (the kubeconfig, signing in): said in its tab until tried again — and, when it's
    /// news, as a warning (whole, and in tarae's log: the tab's line is cut).
    fn fail(&mut self, why: String) -> Vec<Action> {
        let news = self.error.as_ref() != Some(&why);
        self.error = Some(why.clone());
        if news { vec![Action::Warning(format!("Kubernetes: {why}"))] } else { Vec::new() }
    }

    // ── What the manifests hold, and what's watched for them ──

    /// A buffer's text, read again (opened, or changed).
    fn file(&mut self, path: String, version: u64, text: &str) -> Vec<Action> {
        // A chart's values: which releases it's for, on its first line, and what the cluster has of each (its
        // templates read for where they go)
        if path.ends_with("/values.yaml") {
            let lines: Vec<&str> = text.lines().collect();
            self.values.insert(path, (version, fields::fields(&lines, 0)));
            let mut out = self.read_templates();
            out.extend(self.render());
            return out;
        }
        // A kustomization: the ConfigMaps and Secrets it generates, on their `name:` lines
        let docs = if helm::is_template(&path) {
            helm::templates(text, &path)
        } else if kustomize::is_kustomization(&path) {
            let generated = |(kind, name, at): (String, String, u64)| Doc {
                lines: at..at + 1,
                at,
                api_version: "v1".into(),
                kind,
                name,
                namespace: None,
                generated: true,
                via: Via::Itself,
                fields: Vec::new(),
            };
            kustomize::parse(&path, text).generates.into_iter().map(generated).collect()
        } else {
            manifest::docs(text)
        };
        if docs.is_empty() {
            let had = self.files.remove(&path).is_some();
            return if had { self.follow() } else { Vec::new() };
        }
        // (a template: its chart's releases rendered, if they aren't — and, if it's read for its values, read again)
        let template = helm::is_template(&path);
        if let Some(read) = self.chart_files.get_mut(&path) {
            *read = docs.clone();
        }
        self.files.insert(path, File { version, docs });
        if template { self.render() } else { self.follow() }
    }

    fn file_closed(&mut self, path: &str) -> Vec<Action> {
        self.sent.remove(path);
        self.values.remove(path);
        if self.files.remove(path).is_some() { self.follow() } else { Vec::new() }
    }

    /// (kind's plural, namespaced) for a document — `Err`: not known (yet, or at all: why).
    fn resolve(&self, d: &Doc) -> Result<(String, bool), Option<String>> {
        match self.served.get(&d.api_version) {
            None | Some(Served::Asking) => Err(None),
            Some(Served::Failed(why)) => Err(Some(why.clone())),
            Some(Served::Kinds(kinds)) => kinds
                .iter()
                .find(|(k, ..)| *k == d.kind)
                .map(|(_, plural, ns)| (plural.clone(), *ns))
                .ok_or_else(|| Some(format!("{} isn't a kind this cluster knows", d.kind))),
        }
    }

    /// What a document's object is watched as (and its pods', if it has any).
    fn watched(&self, d: &Doc) -> Option<(Watched, Option<Watched>)> {
        let (plural, namespaced) = self.resolve(d).ok()?;
        let ns = namespaced.then(|| d.namespace.clone().unwrap_or_else(|| self.namespace.clone()));
        let pods = (WITH_PODS.contains(&d.kind.as_str()) || d.kind == "Pod")
            .then(|| Watched { api_version: "v1".into(), plural: "pods".into(), ns: ns.clone() });
        Some((Watched { api_version: d.api_version.clone(), plural, ns }, pods))
    }

    /// Asks the cluster about the API versions the manifests use, watches what they hold (and stops
    /// watching what they no longer do), then shows it.
    fn follow(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        if !self.on || self.cluster.is_none() {
            return out;
        }
        let docs: Vec<Doc> = self.watch_docs();
        // (and the objects they name: whether they're there)
        let named: Vec<Doc> = docs.iter().flat_map(|d| self.named(d)).map(|(_, n)| n).collect();
        for d in docs.iter().chain(&named) {
            if !self.served.contains_key(&d.api_version) {
                self.served.insert(d.api_version.clone(), Served::Asking);
                let path = if d.api_version.contains('/') {
                    format!("/apis/{}", d.api_version)
                } else {
                    format!("/api/{}", d.api_version)
                };
                let req = http::Request::get(path).body(Vec::new()).expect("a request");
                out.extend(self.request(Purpose::Discover(d.api_version.clone()), req, None));
            }
        }
        // What's wanted: each kind in a namespace, and the names the manifests have of it; the pods of each
        // namespace that has workloads, by their selectors (none yet: a workload not seen yet)
        let mut kinds: BTreeMap<Watched, (String, BTreeSet<String>)> = BTreeMap::new();
        let mut pods: BTreeMap<Watched, BTreeSet<Select>> = BTreeMap::new();
        for d in &docs {
            let Some((w, p)) = self.watched(d) else { continue };
            kinds.entry(w).or_insert_with(|| (d.kind.clone(), BTreeSet::new())).1.insert(d.name.clone());
            let Some(p) = p.filter(|_| d.kind != "Pod") else { continue };
            let selector = self.live(d).map(|o| status::label_selector(&o.data["spec"]["selector"]));
            let here = pods.entry(p).or_default();
            match selector {
                Some(Some(labels)) => here.insert(Select::Labels(labels)),
                // (one no label selector says: all of the namespace's)
                Some(None) => here.insert(Select::All),
                None => false,
            };
        }
        for n in &named {
            if let Some((w, _)) = self.watched(n) {
                kinds.entry(w).or_insert_with(|| (n.kind.clone(), BTreeSet::new())).1.insert(n.name.clone());
            }
        }
        // (one name: only it — the server sends nothing else; a namespace's pods, by up to two selectors)
        let mut wanted: BTreeMap<(Watched, Select), (String, BTreeSet<String>)> = BTreeMap::new();
        for (w, (kind, names)) in kinds {
            let select = if names.len() == 1 { Select::Name(names.first().cloned().unwrap_or_default()) } else { Select::All };
            wanted.insert((w, select), (kind, names));
        }
        for (w, selects) in pods {
            let selects = if selects.len() > 2 || selects.contains(&Select::All) { [Select::All].into() } else { selects };
            for select in selects {
                wanted.entry((w.clone(), select)).or_insert_with(|| ("Pod".into(), BTreeSet::new()));
            }
        }
        let gone: Vec<u64> = self
            .streams
            .iter()
            .filter(|(_, s)| !wanted.contains_key(&(s.what.clone(), s.select.clone())))
            .map(|(id, _)| *id)
            .collect();
        for sid in gone {
            out.extend(self.drop_stream(sid));
        }
        for ((w, select), (kind, names)) in wanted {
            // (the names an open manifest has may change without the stream changing)
            if let Some(s) = self.streams.values_mut().find(|s| s.what == w && s.select == select) {
                s.names = if kind == "Pod" && s.names.is_empty() { BTreeSet::new() } else { names };
                continue;
            }
            if self.streams.len() >= MAX_STREAMS {
                continue;
            }
            let names = if kind == "Pod" { BTreeSet::new() } else { names };
            self.stream_ids += 1;
            let sid = self.stream_ids;
            let metadata_only = kind == "Secret";
            self.streams.insert(
                sid,
                Stream {
                    what: w,
                    select,
                    names,
                    kind,
                    metadata_only,
                    objects: BTreeMap::new(),
                    listing: None,
                    loaded: false,
                    version: String::new(),
                    watch: None,
                    watch_started: 0,
                    relist: false,
                    retry: 0,
                    error: None,
                },
            );
            out.extend(self.list(sid));
        }
        out.extend(self.show());
        out
    }

    /// A file's documents as the cluster has them: as written — or, when a kustomization takes the file in,
    /// as the build of its root the cluster has most of made them (the same documents, in order).
    fn docs(&self, path: &str) -> Vec<Doc> {
        let Some(f) = self.files.get(path) else { return Vec::new() };
        let readings = self.readings(path, &f.docs);
        if !readings.is_empty() {
            let present = |docs: &Vec<Doc>| docs.iter().filter(|d| self.present(d)).count();
            // (the most there; on a tie, the first root)
            let best = readings.iter().enumerate().max_by_key(|(i, r)| (present(r), std::cmp::Reverse(*i)));
            return best.map(|(_, r)| r.clone()).unwrap_or_default();
        }
        if helm::is_template(path) {
            let via = self.unrendered(path);
            return f.docs.iter().map(|d| Doc { via: via.clone(), ..d.clone() }).collect();
        }
        let roots = self.tree.roots_of(path);
        if roots.is_empty() {
            return f.docs.clone();
        }
        // Taken in by a kustomization, but no build to read it through (yet)
        let why = roots.iter().find_map(|r| match self.builds.get(r) {
            Some(Build::Failed(why)) => Some(why.clone()),
            _ => None,
        });
        let via = match why.or_else(|| self.no_kustomize.clone()) {
            Some(why) => Via::Unbuilt(why),
            None => Via::Building,
        };
        f.docs.iter().map(|d| Doc { via: via.clone(), ..d.clone() }).collect()
    }

    /// How each built root that takes `path` in made its documents.
    fn readings(&self, path: &str, docs: &[Doc]) -> Vec<Vec<Doc>> {
        let mut out = Vec::new();
        for root in self.tree.roots_of(path) {
            let Some(Build::Done(built)) = self.builds.get(&root) else { continue };
            let label = root.rsplit('/').next().unwrap_or(&root).to_string();
            let reading = docs
                .iter()
                .map(|d| match kustomize::became(built, path, &d.kind, &d.name, d.generated) {
                    Some(b) => Doc {
                        api_version: b.api_version.clone(),
                        name: b.name.clone(),
                        namespace: b.namespace.clone(),
                        via: Via::Built(label.clone()),
                        ..d.clone()
                    },
                    None => Doc { via: Via::Unbuilt(format!("{label} doesn't build it")), ..d.clone() },
                })
                .collect();
            out.push(reading);
        }
        // A chart's template: as each release's render made it
        let Some((top, source)) = self.charts.as_ref().and_then(|c| helm::source_of(c, path)) else {
            return out;
        };
        let kinds: Vec<&str> = docs.iter().map(|d| d.kind.as_str()).collect();
        for rel in self.releases_of(&top) {
            let Some(Build::Done(rendered)) = self.renders.get(&(top.clone(), rel.clone())) else { continue };
            let reading = docs
                .iter()
                .zip(helm::became(rendered, &source, &kinds))
                .map(|(d, r)| match r {
                    Some(r) if r.hook.is_some() => {
                        let hook = r.hook.as_deref().unwrap_or_default();
                        Doc {
                            name: r.name.clone(),
                            via: Via::Unbuilt(format!("a Helm hook ({hook}): made when it runs")),
                            ..d.clone()
                        }
                    }
                    Some(r) => Doc {
                        api_version: r.api_version.clone(),
                        name: r.name.clone(),
                        namespace: Some(r.namespace.clone().unwrap_or_else(|| rel.namespace.clone())),
                        via: Via::Built(rel.name.clone()),
                        ..d.clone()
                    },
                    None => {
                        // (with the values it was given — or, not known, with the chart's)
                        let with = match self.release_values.get(&(top.clone(), rel.clone())) {
                            Some(Given::Got(..) | Given::Spent(_) | Given::Nothing) => String::new(),
                            _ => " (with the chart's own values)".into(),
                        };
                        Doc { via: Via::Unbuilt(format!("{} doesn't render it{with}", rel.name)), ..d.clone() }
                    }
                })
                .collect();
            out.push(reading);
        }
        out
    }

    /// Why a template isn't read through a render (yet).
    fn unrendered(&self, path: &str) -> Via {
        let Some(charts) = &self.charts else { return Via::Building };
        let Some((top, _)) = helm::source_of(charts, path) else {
            return Via::Unbuilt("a template, but not in a chart (no Chart.yaml)".into());
        };
        if self.labeled.is_none() {
            return Via::Building;
        }
        let rels = self.releases_of(&top);
        if rels.is_empty() {
            return Via::Unbuilt(format!("no release of {} in {}", charts[&top], self.context()));
        }
        if let Some(why) = &self.no_helm {
            return Via::Unbuilt(why.clone());
        }
        let failed = rels.iter().find_map(|r| match self.renders.get(&(top.clone(), r.clone())) {
            Some(Build::Failed(why)) => Some(why.clone()),
            _ => None,
        });
        failed.map_or(Via::Building, Via::Unbuilt)
    }

    /// The releases the cluster has of the chart in folder `top`.
    fn releases_of(&self, top: &str) -> Vec<helm::Release> {
        match (&self.labeled, self.charts.as_ref().and_then(|c| c.get(top))) {
            (Some(labeled), Some(name)) => helm::releases(labeled, name),
            _ => Vec::new(),
        }
    }

    /// Every document to watch for: each open file's — through every built root that takes it in.
    fn watch_docs(&self) -> Vec<Doc> {
        let mut out = Vec::new();
        for (path, f) in &self.files {
            let readings = self.readings(path, &f.docs);
            if readings.is_empty() {
                out.extend(self.docs(path));
            } else {
                out.extend(readings.into_iter().flatten());
            }
        }
        // (a chart's templates not open, read for where its open values go)
        for (path, docs) in self.chart_files.iter().filter(|(p, _)| !self.files.contains_key(*p)) {
            out.extend(self.readings(path, docs).into_iter().flatten());
        }
        out.into_iter().filter(|d| matches!(d.via, Via::Itself | Via::Built(_))).collect()
    }

    /// The cluster's object for a document, if it has it.
    fn live(&self, d: &Doc) -> Option<&DynamicObject> {
        let (w, _) = self.watched(d)?;
        self.stream_for(&w, &d.name)?.objects.get(&d.name)
    }

    /// Where the cluster's object says otherwise than a line: the cluster's value beside it (`→ 2`). Only
    /// lines read for sure, fields the cluster has, and not a Secret's (its values aren't read).
    fn differ(&self, d: &Doc) -> Vec<Hint> {
        if !matches!(d.via, Via::Itself | Via::Built(_)) || d.kind == "Secret" {
            return Vec::new();
        }
        let Some(live) = self.live(d).and_then(|o| serde_json::to_value(o).ok()) else { return Vec::new() };
        d.fields
            .iter()
            .filter(|f| !f.nested)
            .filter(|f| !matches!(f.path.as_slice(), [fields::Seg::Key(k)] if k == "apiVersion" || k == "kind"))
            .filter_map(|f| {
                let now = fields::at(&live, &f.path)?;
                // (a template's: what it made, said as it is)
                if f.templated {
                    return Some(Hint {
                        line: f.line,
                        text: format!("= {}", fields::told(&now)),
                        tone: Tone::Info,
                    });
                }
                if fields::same(&f.path, &f.value, &now) {
                    return None;
                }
                // (and who set it: the manager that wrote it last)
                let by = fields::managers(&live, &f.path)
                    .into_iter()
                    .next()
                    .map_or(String::new(), |m| format!("  · {m}"));
                Some(Hint { line: f.line, text: format!("→ {}{by}", fields::told(&now)), tone: Tone::Info })
            })
            .collect()
    }

    /// The objects a document's lines name, each as a document of its own (in the document's namespace, under
    /// the name a Kustomize build gave it).
    fn named(&self, d: &Doc) -> Vec<(refs::Ref, Doc)> {
        if !matches!(d.via, Via::Itself | Via::Built(_)) {
            return Vec::new();
        }
        let Some((w, _)) = self.watched(d) else { return Vec::new() };
        let Some(ns) = w.ns else { return Vec::new() };
        // A build's names: what it made of a name (a prefix, a generator's hash)
        let built = match &d.via {
            Via::Built(label) => self.builds.iter().find_map(|(root, b)| match b {
                Build::Done(built) if root.rsplit('/').next() == Some(label.as_str()) => Some(built),
                _ => None,
            }),
            _ => None,
        };
        refs::refs(&d.fields)
            .into_iter()
            .map(|r| {
                let name = built
                    .and_then(|b| {
                        let mine = b.iter().filter(|o| o.kind == r.kind);
                        let exact = b.iter().find(|o| o.kind == r.kind && o.name == r.name);
                        exact.or_else(|| mine.filter(|o| o.name.contains(&r.name)).min_by_key(|o| o.name.len()))
                    })
                    .map_or(r.name.clone(), |o| o.name.clone());
                let doc = Doc {
                    lines: r.line..r.line + 1,
                    at: r.line,
                    api_version: r.api_version.clone(),
                    kind: r.kind.clone(),
                    name,
                    namespace: Some(ns.clone()),
                    generated: false,
                    via: Via::Itself,
                    fields: Vec::new(),
                };
                (r, doc)
            })
            .collect()
    }

    /// Where a line names an object the cluster hasn't — or a key its ConfigMap hasn't, or a Service's
    /// selector no pod has. (Not on a line that already says why its pods aren't fine.)
    fn missing(&self, d: &Doc, said: &[u64]) -> Vec<Hint> {
        let mut out = Vec::new();
        for (r, n) in self.named(d) {
            if said.contains(&r.line) {
                continue;
            }
            // (only once the cluster's been asked)
            let Some((w, _)) = self.watched(&n) else { continue };
            let Some(s) = self.stream_for(&w, &n.name).filter(|s| s.loaded) else { continue };
            let ns = n.namespace.clone().unwrap_or_default();
            match s.objects.get(&n.name) {
                None => {
                    // One of the open manifests has it: it isn't applied yet
                    let ours = self.files.keys().flat_map(|p| self.docs(p)).any(|o| {
                        o.kind == n.kind && o.name == n.name && self.watched(&o).is_some_and(|(w, _)| w.ns.as_deref() == Some(&ns))
                    });
                    let (text, tone) = if ours {
                        (format!("{} {} isn't applied yet", n.kind, n.name), Tone::Info)
                    } else {
                        (format!("no {} {} in {ns}", n.kind, n.name), Tone::Warning)
                    };
                    out.push(Hint { line: r.line, text, tone });
                }
                Some(o) => {
                    if let Some(key) = &r.key {
                        let has = ["data", "binaryData"].iter().any(|f| o.data[f].get(key).is_some());
                        if !has {
                            out.push(Hint { line: r.line, text: format!("no key {key} in {}", n.name), tone: Tone::Warning });
                        }
                    }
                }
            }
        }
        // A Service whose selector no pod has — once the pods it selects (or all the namespace's) are in
        let selector = [fields::Seg::Key("spec".into()), fields::Seg::Key("selector".into())];
        if d.kind == "Service"
            && let Some(o) = self.live(d)
            && let Some(labels) = status::label_selector(&o.data["spec"]["selector"])
            && let Some((_, Some(p))) = self.watched(d)
            && self.streams.values().any(|s| {
                s.what == p && s.loaded && (s.select == Select::All || s.select == Select::Labels(labels.clone()))
            })
            && self.pods_of(d).is_empty()
            && let Some(line) = d.fields.iter().find(|f| f.path == selector).map(|f| f.line)
            && !said.contains(&line)
        {
            out.push(Hint { line, text: "selects no pods".into(), tone: Tone::Warning });
        }
        out
    }

    /// Whether the cluster has a document's object.
    fn present(&self, d: &Doc) -> bool {
        self.watched(d).and_then(|(w, _)| self.stream_for(&w, &d.name)).is_some_and(|s| s.objects.contains_key(&d.name))
    }

    // ── Kustomize ──

    /// Looks for the project's kustomizations again (and, the first time, where its own folder is).
    fn scan(&mut self) -> Vec<Action> {
        let mut out = vec![
            Action::Find((FIND_KZ, "**/kustomization.yaml".into())),
            Action::Find((FIND_CHARTS, "**/Chart.yaml".into())),
            Action::Find((FIND_TEMPLATES, "**/templates/**/*.yaml".into())),
        ];
        if self.own_dir.is_none() {
            let note = b"The Kubernetes plugin's own files: the kustomizations it builds your roots through.\n";
            out.push(Action::WriteOwn((OWN_DIR, "README".into(), note.to_vec())));
        }
        out
    }

    fn found(&mut self, paths: Vec<String>) -> Vec<Action> {
        let reads: Vec<Action> =
            paths.iter().enumerate().map(|(i, p)| Action::ReadFile((KZ_READ + i as u64, p.clone()))).collect();
        let n = paths.len();
        self.reading = (paths, kustomize::Tree::default(), n);
        if n == 0 { self.kz_read_done() } else { reads }
    }

    /// Kustomization `i` read (none: it couldn't be).
    fn kz_read(&mut self, i: usize, text: Option<&[u8]>) -> Vec<Action> {
        let Some(path) = self.reading.0.get(i).cloned() else { return Vec::new() };
        if let Some(t) = text {
            let k = kustomize::parse(&path, &String::from_utf8_lossy(t));
            self.reading.1.kzs.insert(k.dir.clone(), k);
        }
        self.reading.2 = self.reading.2.saturating_sub(1);
        if self.reading.2 == 0 { self.kz_read_done() } else { Vec::new() }
    }

    fn kz_read_done(&mut self) -> Vec<Action> {
        self.tree = std::mem::take(&mut self.reading.1);
        // (built again: what they say may have changed)
        self.builds.clear();
        self.build()
    }

    /// Builds the roots the open files end up in, that aren't built or building.
    fn build(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        if let (Some(own), None) = (self.own_dir.clone(), &self.no_kustomize) {
            let wanted: std::collections::BTreeSet<String> = self
                .files
                .keys()
                .flat_map(|p| self.tree.roots_of(p))
                .filter(|r| !self.builds.contains_key(r))
                .collect();
            for root in wanted {
                let n = self.roots.iter().position(|r| *r == root).unwrap_or_else(|| {
                    self.roots.push(root.clone());
                    self.roots.len() - 1
                });
                let wrapper = kustomize::wrapper(&format!("{own}/kz/{n}"), &root).into_bytes();
                self.builds.insert(root, Build::Running(Vec::new()));
                out.push(Action::WriteOwn((KZ_WRITE + n as u64, format!("kz/{n}/kustomization.yaml"), wrapper)));
            }
        }
        out.extend(self.follow());
        out
    }

    /// Root `n`'s wrapper written (at `path`): built.
    fn kz_written(&mut self, n: usize, path: &str) -> Vec<Action> {
        let args = ["build", "--load-restrictor", "LoadRestrictionsNone", kustomize::parent(path)];
        let args = args.iter().map(|a| a.to_string()).collect();
        vec![Action::Spawn(Spawn { id: KZ_BUILD + n as u64, program: "kustomize".into(), args, env: Vec::new() })]
    }

    fn kz_lines(&mut self, n: usize, lines: Vec<String>) {
        if let Some(Build::Running(out)) = self.roots.get(n).and_then(|r| self.builds.get_mut(r)) {
            out.extend(lines);
        }
    }

    /// Root `n` built (or not).
    fn kz_built(&mut self, n: usize, code: Option<u64>, err: &[String]) -> Vec<Action> {
        let Some(root) = self.roots.get(n).cloned() else { return Vec::new() };
        let Some(Build::Running(out)) = self.builds.remove(&root) else { return Vec::new() };
        let wrap = format!("{}/kz/{n}", self.own_dir.clone().unwrap_or_default());
        let said = err.iter().rev().find(|l| !l.trim().is_empty()).map_or("it failed", |l| l.trim());
        let b = match code {
            Some(0) => Build::Done(kustomize::built(&out.join("\n"), &wrap)),
            _ => Build::Failed(format!("kustomize: {said}")),
        };
        self.builds.insert(root, b);
        self.follow()
    }

    /// Root `n` couldn't be built: why — kustomize not installed is offered (once).
    fn kz_failed(&mut self, n: usize, why: String) -> Vec<Action> {
        if why.contains("isn't on PATH") {
            let first = self.no_kustomize.is_none();
            self.no_kustomize = Some("kustomize isn't installed".into());
            self.builds.retain(|_, b| !matches!(b, Build::Running(_)));
            let mut out = self.follow();
            if first {
                out.push(Action::OfferInstall((KZ_INSTALL, "kustomize".into())));
            }
            return out;
        }
        if let Some(root) = self.roots.get(n).cloned() {
            self.builds.insert(root, Build::Failed(why));
        }
        self.follow()
    }

    // ── Helm ──

    fn found_charts(&mut self, paths: Vec<String>) -> Vec<Action> {
        let reads: Vec<Action> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| Action::ReadFile((HELM_READ + i as u64, p.clone())))
            .collect();
        let n = paths.len();
        self.chart_reading = (paths, BTreeMap::new(), n);
        if n == 0 { self.charts_read() } else { reads }
    }

    /// `Chart.yaml` `i` read (none: it couldn't be).
    fn chart_read(&mut self, i: usize, text: Option<&[u8]>) -> Vec<Action> {
        let Some(path) = self.chart_reading.0.get(i).cloned() else { return Vec::new() };
        if let Some(name) = text.and_then(|t| helm::chart_name(&String::from_utf8_lossy(t))) {
            self.chart_reading.1.insert(kustomize::parent(&path).to_string(), name);
        }
        self.chart_reading.2 = self.chart_reading.2.saturating_sub(1);
        if self.chart_reading.2 == 0 { self.charts_read() } else { Vec::new() }
    }

    fn charts_read(&mut self) -> Vec<Action> {
        self.charts = Some(std::mem::take(&mut self.chart_reading.1));
        self.renders.clear();
        self.release_values.clear();
        // What the cluster has of the charts, looked at afresh — if there are any (else: six lists of the
        // whole cluster for nothing)
        if self.charts.as_ref().is_some_and(|c| c.is_empty()) {
            self.labeled = Some(Vec::new());
            return self.render();
        }
        self.labeling = (self.labeling.0 + 1, Vec::new());
        let mut out = self.label(0, true);
        out.extend(self.read_templates());
        out.extend(self.render());
        out
    }

    /// Looks at the objects of `LABELED[i]` for Helm's labels.
    fn label(&mut self, i: usize, wide: bool) -> Vec<Action> {
        let Some((api_version, plural)) = LABELED.get(i) else {
            self.labeled = Some(std::mem::take(&mut self.labeling.1));
            return self.render();
        };
        let ns = (!wide).then(|| self.namespace.clone());
        let w = Watched { api_version: api_version.to_string(), plural: plural.to_string(), ns };
        let params = ListParams::default().labels("helm.sh/chart").limit(500);
        let accept = "application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1,application/json";
        match kube_core::Request::new(w.url("")).list(&params) {
            Ok(r) => self.request(Purpose::Labeled(self.labeling.0, i, wide), r, Some(accept)),
            Err(_) => self.label(i + 1, true),
        }
    }

    /// Renders each release of a chart an open template is in, that isn't rendered or rendering.
    fn render(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        if let (Some(charts), None, Some(_)) = (&self.charts, &self.no_helm, &self.labeled) {
            let values = self.values.keys().filter_map(|p| helm::source_of(charts, &format!("{}/templates/-", kustomize::parent(p))));
            let tops: std::collections::BTreeSet<String> =
                self.files.keys().filter_map(|p| helm::source_of(charts, p)).chain(values).map(|(top, _)| top).collect();
            for top in tops {
                for rel in self.releases_of(&top) {
                    let key = (top.clone(), rel.clone());
                    if self.renders.contains_key(&key) {
                        continue;
                    }
                    let n = self.render_keys.iter().position(|k| *k == key).unwrap_or_else(|| {
                        self.render_keys.push(key.clone());
                        self.render_keys.len() - 1
                    });
                    // What the release was given first — it renders with those
                    let file = match self.release_values.get(&key) {
                        None => {
                            self.release_values.insert(key, Given::Asking(Vec::new()));
                            let args = ["get", "values", &rel.name, "--namespace", &rel.namespace, "--output", "json"];
                            let args = args.iter().map(|a| a.to_string()).collect();
                            let id = VALUES_GET + n as u64;
                            out.push(Action::Spawn(Spawn { id, program: "helm".into(), args, env: Vec::new() }));
                            continue;
                        }
                        Some(Given::Asking(_) | Given::Writing(_)) => continue,
                        Some(Given::Spent(v)) => {
                            let bytes = serde_json::to_vec(v).unwrap_or_default();
                            self.release_values.insert(key, Given::Writing(v.clone()));
                            out.push(Action::WriteOwn((VALUES_WRITE + n as u64, format!("values/{n}.json"), bytes)));
                            continue;
                        }
                        Some(Given::Got(_, file)) => Some(file.clone()),
                        Some(Given::Nothing | Given::Unknown(_)) => None,
                    };
                    self.renders.insert(key, Build::Running(Vec::new()));
                    let mut args: Vec<String> =
                        ["template", &rel.name, &top, "--namespace", &rel.namespace].iter().map(|a| a.to_string()).collect();
                    if let Some(f) = file {
                        args.extend(["--values".to_string(), f]);
                    }
                    out.push(Action::Spawn(Spawn {
                        id: HELM_RENDER + n as u64,
                        program: "helm".into(),
                        args,
                        env: Vec::new(),
                    }));
                }
            }
        }
        out.extend(self.follow());
        out
    }

    /// `helm get values` for release `n` answered (or not): its values written for its render — or, none or
    /// not known, rendered with the chart's.
    fn values_got(&mut self, n: usize, code: Option<u64>, err: &[String]) -> Vec<Action> {
        let Some(key) = self.render_keys.get(n).cloned() else { return Vec::new() };
        let Some(Given::Asking(out)) = self.release_values.get(&key).cloned() else { return Vec::new() };
        let said = err.iter().rev().find(|l| !l.trim().is_empty()).map_or("it failed", |l| l.trim());
        let given = match (code, serde_json::from_str::<Value>(&out.join("\n"))) {
            (Some(0), Ok(Value::Null)) => Given::Nothing,
            (Some(0), Ok(v)) if self.own_dir.is_some() => Given::Writing(v),
            (Some(0), _) => Given::Unknown("its values couldn't be read".into()),
            _ => Given::Unknown(format!("helm get values: {said}")),
        };
        let mut out = Vec::new();
        if let Given::Writing(v) = &given {
            let bytes = serde_json::to_vec(v).unwrap_or_default();
            out.push(Action::WriteOwn((VALUES_WRITE + n as u64, format!("values/{n}.json"), bytes)));
        }
        self.release_values.insert(key, given);
        out.extend(self.render());
        out
    }

    /// Release `n`'s values written (at `path`): it renders with them.
    fn values_written(&mut self, n: usize, path: Option<String>) -> Vec<Action> {
        let Some(key) = self.render_keys.get(n).cloned() else { return Vec::new() };
        let Some(Given::Writing(v)) = self.release_values.get(&key).cloned() else { return Vec::new() };
        let given = match path {
            Some(p) => Given::Got(v, p),
            None => Given::Unknown("its values couldn't be written for its render".into()),
        };
        self.release_values.insert(key, given);
        self.render()
    }

    /// A chart's file saved: Helm renders what's on disk, so its releases are rendered again — and what they
    /// were given asked again (one may have been upgraded meanwhile).
    fn chart_saved(&mut self, path: &str) {
        let Some(charts) = &self.charts else { return };
        let Some(dir) = charts.keys().find(|d| path.starts_with(&format!("{d}/"))) else { return };
        let Some((top, _)) = helm::source_of(charts, &format!("{dir}/templates/-")) else { return };
        self.renders.retain(|(t, _), _| *t != top);
        self.release_values.retain(|(t, _), _| *t != top);
    }

    /// What release `rel` of the chart in `top` was given at `keys`, if it was given that.
    fn given(&self, top: &str, rel: &str, keys: &[String]) -> Option<Value> {
        let (_, g) = self.release_values.iter().find(|((t, r), _)| t == top && r.name == rel)?;
        let (Given::Got(v, _) | Given::Spent(v)) = g else { return None };
        keys.iter().try_fold(v, |v, k| v.get(k)).cloned()
    }

    fn render_lines(&mut self, n: usize, lines: Vec<String>) {
        if let Some(Build::Running(out)) = self.render_keys.get(n).and_then(|k| self.renders.get_mut(k)) {
            out.extend(lines);
        }
    }

    /// Release `n` rendered (or not).
    fn rendered(&mut self, n: usize, code: Option<u64>, err: &[String]) -> Vec<Action> {
        let Some(key) = self.render_keys.get(n).cloned() else { return Vec::new() };
        let Some(Build::Running(out)) = self.renders.remove(&key) else { return Vec::new() };
        let said = err.iter().rev().find(|l| !l.trim().is_empty()).map_or("it failed", |l| l.trim());
        let b = match code {
            Some(0) => Build::Done(helm::rendered(&out.join("\n"))),
            _ => Build::Failed(format!("helm: {said}")),
        };
        // Its values' file, read: emptied (they're kept here, for what they say)
        let mut wipe = Vec::new();
        if let Some(Given::Got(v, _)) = self.release_values.get(&key).cloned() {
            self.release_values.insert(key.clone(), Given::Spent(v));
            wipe.push(Action::WriteOwn((VALUES_WRITE + n as u64, format!("values/{n}.json"), b"{}".to_vec())));
        }
        self.renders.insert(key, b);
        let mut out = self.follow();
        out.extend(wipe);
        out
    }

    /// Release `n` couldn't be rendered: why — helm not installed is offered (once).
    fn render_failed(&mut self, n: usize, why: String) -> Vec<Action> {
        if why.contains("isn't on PATH") {
            let first = self.no_helm.is_none();
            self.no_helm = Some("helm isn't installed".into());
            self.renders.retain(|_, b| !matches!(b, Build::Running(_)));
            let mut out = self.follow();
            if first {
                out.push(Action::OfferInstall((HELM_INSTALL, "helm".into())));
            }
            return out;
        }
        if let Some(key) = self.render_keys.get(n).cloned() {
            self.renders.insert(key, Build::Failed(why));
        }
        self.follow()
    }

    /// The chart a `values.yaml` is the values of, if it's one's (its folder, and whether it's the top chart's
    /// own — a subchart's are its parent's to give).
    fn values_chart(&self, path: &str) -> Option<(String, bool)> {
        let dir = kustomize::parent(path);
        let (top, _) = helm::source_of(self.charts.as_ref()?, &format!("{dir}/templates/-"))?;
        let own = top == dir;
        Some((top, own))
    }

    /// Reads the templates of each chart whose `values.yaml` is open, that aren't read (or being).
    fn read_templates(&mut self) -> Vec<Action> {
        let tops: BTreeSet<String> =
            self.values.keys().filter_map(|p| self.values_chart(p)).filter(|(_, own)| *own).map(|(t, _)| t).collect();
        let mut out = Vec::new();
        for path in &self.templates {
            let mine = tops.iter().any(|t| path.starts_with(&format!("{t}/templates/")));
            if mine && !self.chart_files.contains_key(path) && !self.tpl_reading.contains(path) {
                self.tpl_reading.push(path.clone());
                out.push(Action::ReadFile((TPL_READ + self.tpl_reading.len() as u64 - 1, path.clone())));
            }
        }
        out
    }

    /// Template `i` read: where its values go.
    fn template_read(&mut self, i: usize, bytes: &[u8]) -> Vec<Action> {
        let Some(path) = self.tpl_reading.get(i).cloned() else { return Vec::new() };
        let docs = helm::templates(&String::from_utf8_lossy(bytes), &path);
        self.chart_files.insert(path, docs);
        self.render()
    }

    /// A chart's `values.yaml`: which releases of it the cluster has (on its first line), and — beside a value a
    /// template writes as it is — what the cluster has there, where it isn't the value (each release's).
    fn values_hints(&self, path: &str) -> Vec<Hint> {
        let mut out = Vec::new();
        let (Some(charts), Some((top, own)), Some(_)) = (&self.charts, self.values_chart(path), &self.labeled) else {
            return out;
        };
        let dir = kustomize::parent(path);
        let name = &charts[&top];
        let rels = self.releases_of(&top);
        let through = if !own { format!(" (through {name})") } else { String::new() };
        let text = match rels.as_slice() {
            [] => format!("no release of {name} in {}", self.context()),
            [r] => format!("release {} · ns {}{through}", r.name, r.namespace),
            rs => {
                let each: Vec<String> = rs.iter().map(|r| format!("{} (ns {})", r.name, r.namespace)).collect();
                format!("releases {}{through}", each.join(", "))
            }
        };
        out.push(Hint { line: 0, text, tone: Tone::Info });
        let Some((_, fields)) = self.values.get(path).filter(|_| own) else { return out };
        // Where each value goes: the chart's templates (as open, or as read), the lines writing it as it is
        let mine = |p: &&String| p.starts_with(&format!("{dir}/templates/"));
        let mut templates: BTreeMap<&String, &Vec<Doc>> =
            self.chart_files.iter().filter(|(p, _)| mine(p)).map(|(p, d)| (p, d)).collect();
        // (as open: as it is now)
        templates.extend(self.files.iter().filter(|(p, _)| mine(p)).map(|(p, f)| (p, &f.docs)));
        for f in fields.iter().filter(|f| !f.nested && !f.templated) {
            let Some(keys) = f.path.iter().map(|s| if let fields::Seg::Key(k) = s { Some(k.clone()) } else { None }).collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            // (release, what the cluster has, who set it) where it isn't the value
            let mut said: Vec<(String, String, Option<String>)> = Vec::new();
            for (tpath, docs) in &templates {
                for u in helm::usages(docs).into_iter().filter(|u| u.values == keys) {
                    for reading in self.readings(tpath, docs) {
                        let Some(d) = reading.get(u.doc) else { continue };
                        let Via::Built(rel) = &d.via else { continue };
                        let Some(live) = self.live(d).and_then(|o| serde_json::to_value(o).ok()) else { continue };
                        let Some(now) = fields::at(&live, &u.path) else { continue };
                        if fields::same(&u.path, &f.value, &now) {
                            continue;
                        }
                        // Who: the release's own values, if they say so — else the field's last manager
                        let given = self.given(&top, rel, &keys).is_some_and(|g| fields::same(&u.path, &g, &now));
                        let by = if given { Some("release values".into()) } else { fields::managers(&live, &u.path).into_iter().next() };
                        let entry = (rel.clone(), fields::told(&now), by);
                        if !said.contains(&entry) {
                            said.push(entry);
                        }
                    }
                }
            }
            if said.is_empty() {
                continue;
            }
            let several = said.iter().map(|(r, ..)| r).collect::<BTreeSet<_>>().len() > 1;
            let each: Vec<String> = said
                .iter()
                .map(|(rel, now, by)| {
                    let by = by.as_ref().map_or(String::new(), |b| format!(" · {b}"));
                    let rel = if several { format!(" ({rel})") } else { String::new() };
                    format!("{now}{by}{rel}")
                })
                .collect();
            out.push(Hint { line: f.line, text: format!("→ {}", each.join(", ")), tone: Tone::Info });
        }
        out
    }

    fn drop_stream(&mut self, sid: u64) -> Vec<Action> {
        let mut out = self.stop_stream(sid);
        self.streams.remove(&sid);
        out.push(Action::Stop(RETRY + sid));
        out
    }

    /// The stream an object named `name` of `w`'s kind and namespace comes in.
    fn stream_for(&self, w: &Watched, name: &str) -> Option<&Stream> {
        self.streams.values().find(|s| s.what == *w && s.select.covers(name))
    }

    /// The pods of a namespace that are watched (the ones its workloads select, or all of them).
    fn pods_in(&self, pods: &Watched) -> Vec<&DynamicObject> {
        let mut out: BTreeMap<&str, &DynamicObject> = BTreeMap::new();
        for s in self.streams.values().filter(|s| s.what == *pods) {
            out.extend(s.objects.iter().map(|(n, o)| (n.as_str(), o)));
        }
        out.into_values().collect()
    }

    /// How a document's object is doing (none: not known yet).
    fn seen(&self, d: &Doc) -> Option<Seen> {
        match &d.via {
            Via::Building => return None,
            Via::Unbuilt(why) => {
                return Some(Seen { text: why.clone(), tone: Tone::Info, brief: String::new() });
            }
            _ => {}
        }
        let (w, pods) = match self.resolve(d) {
            Err(None) => return None,
            Err(Some(why)) => return Some(Seen { text: why, tone: Tone::Warning , brief: String::new() }),
            Ok(_) => self.watched(d)?,
        };
        let Some(s) = self.stream_for(&w, &d.name) else {
            return Some(Seen { text: "not watched: too many kinds open".into(), tone: Tone::Info , brief: String::new() });
        };
        if let Some(e) = &s.error
            && !s.loaded
        {
            return Some(Seen { text: e.clone(), tone: Tone::Warning , brief: String::new() });
        }
        if !s.loaded {
            return None;
        }
        let Some(o) = s.objects.get(&d.name) else {
            return Some(Seen { text: format!("not in {}", self.context()), tone: Tone::Warning , brief: String::new() });
        };
        let pods: Vec<&DynamicObject> = pods.map(|p| self.pods_in(&p)).unwrap_or_default();
        let (mut text, tone) = status::summary(&d.kind, o, &pods);
        let brief = status::brief(&d.kind, o, &pods);
        // A namespace it didn't say: the one it was looked for in · the root it was built through
        if d.namespace.is_none() && w.ns.is_some() {
            text.push_str(&format!("  (ns {}, from the context)", self.namespace));
        }
        if let Via::Built(root) = &d.via {
            text.push_str(&format!("  (via {root})"));
        }
        Some(Seen { text, tone, brief })
    }

    /// A buffer is now in front (none: one with no file): the side tab is its objects — a manifest it hasn't
    /// read yet (open before it was on), read now.
    fn shown(&mut self, path: Option<String>, version: u64, text: Option<&str>) -> Vec<Action> {
        self.front = path.clone();
        let mut out = Vec::new();
        if let (Some(path), Some(text)) = (path, text)
            && self.files.get(&path).map(|f| f.version) != Some(version)
            && !self.values.contains_key(&path)
        {
            out.extend(self.file(path, version, text));
        }
        out.extend(self.show());
        out
    }

    /// The hints, the side tab and the badge, as things are — sent where they changed.
    fn show(&mut self) -> Vec<Action> {
        let mut out = Vec::new();
        if !self.on {
            return out;
        }
        let mut changed = Vec::new();
        for (path, f) in &self.files {
            let mut hints = Vec::new();
            let now = tarae_plugin::now::millis() as i64;
            for d in self.docs(path) {
                if let Some(s) = self.seen(&d) {
                    hints.push(Hint { line: d.at, text: s.text, tone: s.tone });
                }
                // Why its pods aren't fine, on the lines that are why — before what the cluster has otherwise
                let pods = self.pods_of(&d);
                let causes = cause::causes(&d.fields, &pods, now);
                let said: Vec<u64> = causes.iter().map(|c| c.line).collect();
                hints.extend(causes.into_iter().map(|c| Hint { line: c.line, text: c.text, tone: c.tone }));
                // What it names that isn't there
                hints.extend(self.missing(&d, &said));
                hints.extend(self.differ(&d));
            }
            let hints = together(hints);
            // (the same hints for an edited buffer: sent again — tarae holds them to the version they're for)
            let now = (f.version, hints);
            if self.sent.get(path) != Some(&now) {
                changed.push((path.clone(), now));
            }
        }
        for (path, (version, _)) in &self.values {
            let now = (*version, together(self.values_hints(path)));
            if self.sent.get(path) != Some(&now) {
                changed.push((path.clone(), now));
            }
        }
        for (path, (version, hints)) in changed {
            self.sent.insert(path.clone(), (version, hints.clone()));
            out.push(Action::Hints(Hints { path, version, hints }));
        }
        let side = self.side_now();
        if self.side.as_ref() != Some(&side) {
            self.side = Some(side.clone());
            // The first time it's on: the side panel opens at its tab
            let open = !std::mem::replace(&mut self.side_opened, true);
            out.push(Action::Side(Some(Side { open, focus: false, ..side })));
        }
        let tone = if self.error.is_some() { Tone::Error } else { Tone::Ok };
        let badge = (format!("⎈ {}", self.context()), tone);
        if self.badge.as_ref() != Some(&badge) {
            self.badge = Some(badge.clone());
            out.push(Action::Badge(Some(badge)));
        }
        out
    }

    /// The side tab: each open manifest, its objects and how they're doing, and under one that isn't
    /// fine the pods that aren't.
    fn side_now(&self) -> Side {
        let row = |id: String, depth, text: String, detail: String, tone, at| SideRow {
            id,
            depth,
            text,
            detail,
            tone,
            at,
            note: false,
        };
        // Why a row isn't fine: a line of its own under it, in its tone's color
        let why = |depth, text: String, tone| SideRow {
            note: true,
            ..row(String::new(), depth, text, String::new(), tone, None)
        };
        let now = tarae_plugin::now::millis() as i64;
        let mut rows = Vec::new();
        if let Some(e) = &self.error {
            rows.push(row("retry".into(), 0, "can't show the cluster".into(), "enter: again".into(), Tone::Error, None));
            // (all of it, a line at a time: the panel is narrow)
            rows.extend(wrapped(e, 30).into_iter().map(|l| why(1, l, Tone::Error)));
        } else if self.cluster.is_none() {
            rows.push(row("none".into(), 0, "connecting…".into(), String::new(), Tone::Info, None));
        } else if self.files.is_empty() {
            let text = "open a manifest: its objects show here".into();
            rows.push(row("none".into(), 0, text, String::new(), Tone::Info, None));
        } else if !self.front.as_ref().is_some_and(|f| self.files.contains_key(f)) {
            let text = "no objects in this file".into();
            rows.push(row("none".into(), 0, text, String::new(), Tone::Info, None));
        }
        if self.cluster.is_some() {
            // The file in front: its objects, by kind, and the pods of those that aren't fine
            for path in self.files.keys().filter(|p| Some(*p) == self.front.as_ref()) {
                let docs = self.docs(path);
                let file = path.rsplitn(3, '/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/");
                let seen: Vec<Seen> = docs
                    .iter()
                    .map(|d| self.seen(d).unwrap_or(Seen { text: "…".into(), tone: Tone::Info, brief: "…".into() }))
                    .collect();
                let tones: Vec<Tone> = seen.iter().map(|s| s.tone).collect();
                let last = docs.last().map_or(0, |d| d.lines.end);
                // (fine: what's known to be — not what isn't known yet)
                let fine = tones.iter().filter(|t| **t == Tone::Ok).count();
                let (detail, worst) = tally(&tones, if fine > 0 { format!("{fine} fine") } else { String::new() });
                rows.push(row(format!("f:{path}"), 0, file, detail, worst, Some((path.clone(), 0, last))));
                // By kind (in the order the file has them), then each by name — a long name keeps its room
                let mut kinds: Vec<&str> = Vec::new();
                for d in &docs {
                    if !kinds.contains(&d.kind.as_str()) {
                        kinds.push(&d.kind);
                    }
                }
                for kind in kinds {
                    let mine: Vec<usize> = (0..docs.len()).filter(|&i| docs[i].kind == kind).collect();
                    let tones: Vec<Tone> = mine.iter().map(|&i| seen[i].tone).collect();
                    let (detail, worst) = tally(&tones, mine.len().to_string());
                    rows.push(row(format!("k:{path}:{kind}"), 1, kind.to_string(), detail, worst, None));
                    for i in mine {
                        let (d, st) = (&docs[i], &seen[i]);
                        let at = Some((path.clone(), d.lines.start, d.lines.end.saturating_sub(1)));
                        rows.push(row(format!("{path}#{i}"), 2, d.name.clone(), st.brief.clone(), st.tone, at));
                        if !matches!(st.tone, Tone::Error | Tone::Warning) {
                            continue;
                        }
                        // Not fine: its pods that aren't (newest first), each with why — or, when no pod
                        // says it, why under itself
                        let ns = self.watched(d).and_then(|(w, _)| w.ns).unwrap_or_else(|| self.namespace.clone());
                        let bad: Vec<&DynamicObject> =
                            self.pods_of(d).into_iter().filter(|p| status::pod_tone(p).1 != Tone::Ok).collect();
                        if bad.is_empty() || d.kind == "Pod" {
                            let reason = st.text.split("  (ns ").next().unwrap_or(&st.text);
                            let reason = reason.split_once(" · ").map_or(reason, |(_, r)| r);
                            rows.push(why(3, reason.to_string(), st.tone));
                        }
                        // (a pod in a manifest is its own row: none under it)
                        for p in bad.into_iter().take(if d.kind == "Pod" { 0 } else { 5 }) {
                            let name = name_of(p);
                            let (state, tone) = status::pod_tone(p);
                            // (under its workload: without the workload's name in front)
                            let short = name.strip_prefix(&format!("{}-", d.name)).unwrap_or(&name).to_string();
                            // Its age, if it's new; its restarts, if any
                            let mut detail = Vec::new();
                            if let Some(t) = p.metadata.creation_timestamp.as_ref().map(|t| t.0.as_millisecond())
                                && now - t < 10 * 60_000
                            {
                                detail.push(status::human(now - t));
                            }
                            match status::pod_restarts(p) {
                                0 => {}
                                n => detail.push(format!("↻{n}")),
                            }
                            rows.push(row(format!("pod:{ns}/{name}"), 3, short, detail.join(" "), tone, None));
                            rows.push(why(4, state, tone));
                        }
                    }
                }
            }
            // The other open files: only those that aren't fine, to go to
            let mut others = Vec::new();
            for path in self.files.keys().filter(|p| Some(*p) != self.front.as_ref()) {
                let docs = self.docs(path);
                let tones: Vec<Tone> = docs.iter().filter_map(|d| self.seen(d)).map(|s| s.tone).collect();
                let (detail, worst) = tally(&tones, String::new());
                if matches!(worst, Tone::Error | Tone::Warning) {
                    let file = path.rsplitn(3, '/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/");
                    let last = docs.last().map_or(0, |d| d.lines.end);
                    others.push(row(format!("f:{path}"), 1, file, detail, worst, Some((path.clone(), 0, last))));
                }
            }
            if !others.is_empty() {
                let worst = others.iter().map(|r| r.tone).min_by_key(|t| rank(*t)).unwrap_or(Tone::Warning);
                rows.push(row("others".into(), 0, "other open files".into(), String::new(), worst, None));
                rows.extend(others);
            }
        }
        let keys = [("ret", "go / logs"), ("d", "describe"), ("e", "events"), ("y", "yaml"), ("a", "all")];
        Side {
            title: "k8s".into(),
            note: format!("⎈ {}", self.context()),
            rows,
            keys: keys.iter().map(|(k, d)| (k.to_string(), d.to_string())).collect(),
            open: false,
            focus: false,
        }
    }

    /// A key on a row of its side tab.
    fn side_key(&mut self, key: &str, id: Option<String>) -> Vec<Action> {
        let Some(id) = id else { return Vec::new() };
        if id == "retry" {
            return if key == "ret" { self.start() } else { Vec::new() };
        }
        if let Some(pod) = id.strip_prefix("pod:") {
            let Some((ns, name)) = pod.split_once('/') else { return Vec::new() };
            let (ns, name) = (ns.to_string(), name.to_string());
            let pods = Watched { api_version: "v1".into(), plural: "pods".into(), ns: Some(ns.clone()) };
            return match key {
                "ret" => self.logs(&ns, &name),
                "d" => self.describe(&ns, &name),
                "e" => self.events(Some(&ns), "Pod", &name),
                "y" => self.yaml(&pods, "Pod", &name),
                _ => Vec::new(),
            };
        }
        let Some((path, i)) = self.doc_at(&id) else { return Vec::new() };
        match key {
            "e" | "y" => {
                self.picking = Some((path, i));
                self.act(if key == "e" { "events" } else { "yaml" })
            }
            "a" => self.pick_action(path, i),
            "d" => match self.docs(&path).get(i).cloned() {
                // A pod in a manifest: described; anything else's is `e` and `y`
                Some(d) if d.kind == "Pod" => {
                    let ns = self.watched(&d).and_then(|(w, _)| w.ns).unwrap_or_else(|| self.namespace.clone());
                    self.describe(&ns, &d.name)
                }
                _ => vec![Action::Status("d describes a pod — e: its events, y: its YAML".into())],
            },
            _ => Vec::new(),
        }
    }

    // ── What to do with an object ──

    fn doc_at(&self, id: &str) -> Option<(String, usize)> {
        let (path, i) = id.rsplit_once('#')?;
        let i: usize = i.parse().ok()?;
        self.docs(path).get(i).map(|_| (path.to_string(), i))
    }

    /// What to do with an object: go to it, its live YAML, its events, its pods' logs.
    fn pick_action(&mut self, path: String, i: usize) -> Vec<Action> {
        let Some(d) = self.docs(&path).get(i).cloned() else { return Vec::new() };
        let file = path.rsplit('/').next().unwrap_or(&path).to_string();
        let mut items = vec![
            ("goto".to_string(), format!("go to {file}:{}", d.at + 1)),
            ("yaml".into(), "live YAML".into()),
            ("events".into(), "events".into()),
        ];
        let pods: Vec<(String, String)> =
            self.pods_of(&d).into_iter().take(12).map(|p| (name_of(p), status::pod_status(p))).collect();
        for (pod, st) in pods {
            items.push((format!("logs:{pod}"), format!("logs {pod} · {st}")));
        }
        self.picking = Some((path, i));
        vec![Action::Pick(Pick { id: PICK_ACTION, title: format!("{} {}", d.kind, d.name), items })]
    }

    /// The pods of a document's object (a workload's, a service's, or itself), newest first.
    fn pods_of(&self, d: &Doc) -> Vec<&DynamicObject> {
        let Some((_, Some(pods))) = self.watched(d) else { return Vec::new() };
        if d.kind == "Pod" {
            return self.live(d).into_iter().collect();
        }
        let Some(o) = self.live(d) else { return Vec::new() };
        let selector = &o.data["spec"]["selector"];
        let mut mine: Vec<&DynamicObject> =
            self.pods_in(&pods).into_iter().filter(|p| status::selects(selector, p)).collect();
        mine.sort_by_key(|p| std::cmp::Reverse(p.metadata.creation_timestamp.as_ref().map(|t| t.0.as_millisecond())));
        mine
    }

    /// A pod as its namespace's stream has it.
    fn pod(&self, ns: &str, name: &str) -> Option<&DynamicObject> {
        let pods = Watched { api_version: "v1".into(), plural: "pods".into(), ns: Some(ns.into()) };
        self.pods_in(&pods).into_iter().find(|p| p.metadata.name.as_deref() == Some(name))
    }

    fn act(&mut self, what: &str) -> Vec<Action> {
        let Some((path, i)) = self.picking.take() else { return Vec::new() };
        let Some(d) = self.docs(&path).get(i).cloned() else { return Vec::new() };
        let Some((w, _)) = self.watched(&d) else { return Vec::new() };
        match what {
            "goto" => vec![Action::Open(format!("{path}:{}", d.at + 1))],
            "yaml" => self.yaml(&w, &d.kind, &d.name),
            "events" => self.events(w.ns.as_deref(), &d.kind, &d.name),
            logs if logs.starts_with("logs:") => {
                let ns = w.ns.unwrap_or_else(|| self.namespace.clone());
                self.logs(&ns, &logs[5..])
            }
            _ => Vec::new(),
        }
    }

    /// An object's live YAML, in a buffer (a Secret's values masked).
    fn yaml(&mut self, w: &Watched, kind: &str, name: &str) -> Vec<Action> {
        match kube_core::Request::new(w.url(kind)).get(name, &GetParams::default()) {
            Ok(r) => {
                let key = format!("yaml {kind} {}/{name}", w.ns.as_deref().unwrap_or(""));
                self.request(Purpose::Yaml(key, format!("{kind} {name}"), kind == "Secret"), r, None)
            }
            Err(e) => vec![Action::Warning(e.to_string())],
        }
    }

    /// An object's events, in a buffer — a workload's with its pods' and replica sets' (where an image
    /// that won't pull is said).
    fn events(&mut self, ns: Option<&str>, kind: &str, name: &str) -> Vec<Action> {
        let ns_path = ns.map_or(String::new(), |n| format!("/namespaces/{n}"));
        let path = if WITH_PODS.contains(&kind) && kind != "Service" {
            format!("/api/v1{ns_path}/events")
        } else {
            format!("/api/v1{ns_path}/events?fieldSelector=involvedObject.name%3D{name},involvedObject.kind%3D{kind}")
        };
        let req = http::Request::get(path).body(Vec::new()).expect("a request");
        let key = format!("events {kind} {}/{name}", ns.unwrap_or(""));
        self.request(Purpose::Events(key, format!("events {kind} {name}"), name.to_string(), String::new()), req, None)
    }

    /// A pod described (its state, containers, how they last ended, conditions), then its events — in a
    /// buffer.
    fn describe(&mut self, ns: &str, pod: &str) -> Vec<Action> {
        let Some(p) = self.pod(ns, pod) else { return vec![Action::Status(format!("{pod}: gone"))] };
        let head = status::describe(p, tarae_plugin::now::millis() as i64);
        let path = format!(
            "/api/v1/namespaces/{ns}/events?fieldSelector=involvedObject.name%3D{pod},involvedObject.kind%3DPod"
        );
        let req = http::Request::get(path).body(Vec::new()).expect("a request");
        let purpose = Purpose::Events(format!("describe {ns}/{pod}"), format!("describe {pod}"), pod.to_string(), head);
        self.request(purpose, req, None)
    }

    /// A pod's logs, followed, in a buffer (the same pod's: the same buffer, started over).
    fn logs(&mut self, ns: &str, pod: &str) -> Vec<Action> {
        let view = self.view(&format!("logs {ns}/{pod}"));
        let mut out = self.view_closed(view);
        let path = format!("/api/v1/namespaces/{ns}/pods/{pod}/log?follow=true&tailLines=200");
        let req = http::Request::get(path).body(Vec::new()).expect("a request");
        out.push(Action::Show(View { id: view, title: format!("logs {pod}"), text: String::new(), language: None }));
        out.extend(self.request(Purpose::Logs(view), req, None));
        out
    }

    /// The buffer id for what it shows (the same object's: the same buffer).
    fn view(&mut self, key: &str) -> u64 {
        if let Some(&v) = self.views.get(key) {
            return v;
        }
        self.view_ids += 1;
        let v = FIRST_VIEW + self.view_ids;
        self.views.insert(key.to_string(), v);
        v
    }

    /// A buffer it showed was closed: the log it followed stops.
    fn view_closed(&mut self, view: u64) -> Vec<Action> {
        let ids: Vec<u64> = self
            .reqs
            .iter()
            .filter(|(_, r)| matches!(r.purpose, Purpose::Logs(v) if v == view))
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .map(|id| {
                self.reqs.remove(&id);
                Action::Stop(id)
            })
            .collect()
    }

    // ── Lists and watches ──

    /// A request on a connection of its own (`accept`: a type other than JSON objects).
    fn request(&mut self, purpose: Purpose, req: http::Request<Vec<u8>>, accept: Option<&str>) -> Vec<Action> {
        let Some(c) = &self.cluster else { return Vec::new() };
        let mut headers: Vec<(&str, String)> =
            vec![("Accept", accept.unwrap_or("application/json").into()), ("Connection", "close".into())];
        if let Some(t) = &c.token {
            headers.push(("Authorization", format!("Bearer {t}")));
        }
        let path = format!("{}{}", c.base, req.uri().path_and_query().map_or("/", |p| p.as_str()));
        let hs: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let host =
            if c.host.contains(':') { format!("[{}]:{}", c.host, c.port) } else { format!("{}:{}", c.host, c.port) };
        let bytes = tarae_plugin::http::request(req.method().as_str(), &path, &host, &hs, req.body());
        self.next = self.next.max(FIRST_CONN) + 1;
        let id = self.next;
        let (host, port) = (c.host.clone(), c.port);
        if let Purpose::Watch(sid) = purpose
            && let Some(s) = self.streams.get_mut(&sid)
        {
            s.watch = Some(id);
        }
        self.reqs.insert(
            id,
            Req { purpose, http: bytes, tls: None, resp: Response::new(), lines: Lines::new(), body: Vec::new() },
        );
        vec![Action::Connect(Connect { id, host, port })]
    }

    fn list(&mut self, sid: u64) -> Vec<Action> {
        let mut out = self.stop_stream(sid);
        out.push(Action::Stop(RETRY + sid));
        let Some(s) = self.streams.get_mut(&sid) else { return out };
        s.listing = Some(BTreeMap::new());
        s.relist = false;
        out.extend(self.list_page(sid, None));
        out
    }

    fn list_page(&mut self, sid: u64, token: Option<String>) -> Vec<Action> {
        let Some(s) = self.streams.get(&sid) else { return Vec::new() };
        let mut params = ListParams::default().limit(500);
        if let Some(t) = &token {
            params = params.continue_token(t);
        }
        match &s.select {
            Select::All => {}
            Select::Name(n) => params = params.fields(&format!("metadata.name={n}")),
            Select::Labels(l) => params = params.labels(l),
        }
        let accept = s
            .metadata_only
            .then_some("application/json;as=PartialObjectMetadataList;g=meta.k8s.io;v=v1,application/json");
        match kube_core::Request::new(s.what.url(&s.kind)).list(&params) {
            Ok(r) => self.request(Purpose::List(sid), r, accept),
            Err(e) => self.again(sid, e.to_string()),
        }
    }

    fn watch(&mut self, sid: u64) -> Vec<Action> {
        let Some(s) = self.streams.get_mut(&sid) else { return Vec::new() };
        s.watch_started = tarae_plugin::now::millis();
        let accept =
            s.metadata_only.then_some("application/json;as=PartialObjectMetadata;g=meta.k8s.io;v=v1,application/json");
        let params = match &s.select {
            Select::All => WatchParams::default(),
            Select::Name(n) => WatchParams::default().fields(&format!("metadata.name={n}")),
            Select::Labels(l) => WatchParams::default().labels(l),
        };
        let req = kube_core::Request::new(s.what.url(&s.kind)).watch(&params.timeout(290), &s.version);
        match req {
            Ok(r) => self.request(Purpose::Watch(sid), r, accept),
            Err(e) => self.again(sid, e.to_string()),
        }
    }

    /// A stream's watch ended: on from where it was — or, if the server said its version is too old, a
    /// fresh list; if it failed or ended as soon as it started, a pause first.
    fn watch_ended(&mut self, sid: u64, why: &str) -> Vec<Action> {
        let Some(s) = self.streams.get_mut(&sid) else { return Vec::new() };
        s.watch = None;
        let brief = tarae_plugin::now::millis().saturating_sub(s.watch_started) < 1000;
        match (why, brief, s.relist) {
            ("", false, false) => self.watch(sid),
            ("", false, true) => self.list(sid),
            ("", true, _) => self.again(sid, "the watch ended as soon as it started".into()),
            (why, ..) => self.again(sid, why.to_string()),
        }
    }

    /// A stream's list or watch failed: said, and tried again from a fresh list after a pause that grows
    /// (1 s, 2 s, 4 s, … 30 s).
    fn again(&mut self, sid: u64, why: String) -> Vec<Action> {
        let mut out = self.stop_stream(sid);
        let Some(s) = self.streams.get_mut(&sid) else { return out };
        s.listing = None;
        s.error = Some(why);
        let ms = (1000u64 << s.retry.min(5)).min(30_000);
        s.retry += 1;
        out.push(Action::Timer((RETRY + sid, ms)));
        out
    }

    /// A stream's connections closed.
    fn stop_stream(&mut self, sid: u64) -> Vec<Action> {
        if let Some(s) = self.streams.get_mut(&sid) {
            s.watch = None;
        }
        let ids: Vec<u64> = self
            .reqs
            .iter()
            .filter(|(_, r)| matches!(r.purpose, Purpose::List(s) | Purpose::Watch(s) if s == sid))
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .map(|id| {
                self.reqs.remove(&id);
                Action::Stop(id)
            })
            .collect()
    }

    /// A change a watch says (and whether it's to what's shown: an object the manifests have, or a pod).
    fn watch_event(&mut self, sid: u64, line: &str) {
        let Some(s) = self.streams.get_mut(&sid) else { return };
        let mine = |s: &Stream, o: &DynamicObject| s.names.is_empty() || s.names.contains(&name_of(o));
        match serde_json::from_str::<WatchEvent<DynamicObject>>(line) {
            Ok(WatchEvent::Added(o) | WatchEvent::Modified(o)) => {
                s.version = o.metadata.resource_version.clone().unwrap_or_default();
                if mine(s, &o) {
                    self.dirty = true;
                    // (a workload: which pods it has may be asked for now)
                    self.refollow |= WITH_PODS.contains(&s.kind.as_str());
                }
                s.objects.insert(name_of(&o), o);
            }
            Ok(WatchEvent::Deleted(o)) => {
                if mine(s, &o) {
                    (self.dirty, self.refollow) = (true, true);
                }
                s.objects.remove(&name_of(&o));
            }
            Ok(WatchEvent::Bookmark(b)) => s.version = b.metadata.resource_version,
            // Too old a version to watch from: a fresh list when this watch ends
            Ok(WatchEvent::Error(e)) if e.code == 410 => s.relist = true,
            Ok(WatchEvent::Error(e)) => (s.error, self.dirty) = (Some(e.message), true),
            Err(e) => (s.error, self.dirty) = (Some(format!("a change: {e}")), true),
        }
    }

    // ── A connection's life ──

    /// Request `id` failed (`status`: the HTTP status, if it got that far).
    fn req_failed(&mut self, id: u64, why: String, status: Option<u16>) -> Vec<Action> {
        let Some(r) = self.reqs.remove(&id) else { return Vec::new() };
        let mut out = vec![Action::Stop(id)];
        // Unauthorized: the token may have expired — a fresh one
        if status == Some(401) {
            out.extend(self.run_exec());
        }
        out.extend(match r.purpose {
            Purpose::Discover(av) => {
                let why = if status == Some(404) { format!("{av} isn't served by this cluster") } else { why };
                self.served.insert(av, Served::Failed(why));
                Vec::new()
            }
            // Not allowed to see them: said, and asked again only in a minute
            Purpose::List(sid) | Purpose::Watch(sid) if status == Some(403) => {
                let mut out = self.stop_stream(sid);
                if let Some(s) = self.streams.get_mut(&sid) {
                    s.error = Some("no access".into());
                    s.loaded = false;
                }
                out.push(Action::Timer((RETRY + sid, 60_000)));
                out
            }
            Purpose::List(sid) | Purpose::Watch(sid) => self.again(sid, why),
            Purpose::Logs(view) => vec![Action::Append((view, format!("\n[{why}]\n")))],
            Purpose::Yaml(..) | Purpose::Events(..) => vec![Action::Warning(why)],
            // Helm's labels: cluster-wide not allowed — the context's namespace; nothing — the next kind
            Purpose::Labeled(look, i, wide) if look == self.labeling.0 => {
                if wide && status == Some(403) {
                    self.label(i, false)
                } else {
                    self.label(i + 1, true)
                }
            }
            Purpose::Labeled(..) => Vec::new(),
        });
        out
    }

    fn connected(&mut self, id: u64) -> Vec<Action> {
        let (Some(c), Some(r)) = (&self.cluster, self.reqs.get_mut(&id)) else { return vec![Action::Stop(id)] };
        let started = Tls::client(c.tls.clone(), &c.server_name).and_then(|mut t| {
            t.write(&r.http)?;
            Ok(t)
        });
        match started {
            Ok(mut t) => {
                let out = t.outgoing();
                r.tls = Some(t);
                vec![Action::Send((id, out))]
            }
            Err(e) => self.req_failed(id, e, None),
        }
    }

    fn data(&mut self, id: u64, bytes: &[u8]) -> Vec<Action> {
        let Some(r) = self.reqs.get_mut(&id) else { return vec![Action::Stop(id)] };
        let Some(t) = r.tls.as_mut() else { return Vec::new() };
        let mut out = Vec::new();
        if let Err(e) = t.feed(bytes) {
            out.extend(self.req_failed(id, e, None));
            return out;
        }
        let send = t.outgoing();
        if !send.is_empty() {
            out.push(Action::Send((id, send)));
        }
        let plain = t.read();
        let body = match r.resp.feed(&plain) {
            Ok(b) => b,
            Err(e) => {
                out.extend(self.req_failed(id, e, None));
                return out;
            }
        };
        let status = r.resp.head().map(|h| h.status);
        if status.is_some_and(|s| s >= 400) {
            self.dirty = true;
            r.body.extend(body);
            if r.resp.done() {
                let why = format!("HTTP {}: {}", status.unwrap_or(0), api_error(&r.body));
                out.extend(self.req_failed(id, why, status));
            }
            return out;
        }
        match r.purpose {
            Purpose::Watch(sid) => {
                let lines = r.lines.feed(&body);
                let done = r.resp.done();
                for l in lines {
                    self.watch_event(sid, &l);
                }
                // (the server ended the response: the watch is over, whether or not the connection closes)
                if done {
                    self.dirty = true;
                    self.reqs.remove(&id);
                    out.push(Action::Stop(id));
                    out.extend(self.watch_ended(sid, ""));
                }
            }
            Purpose::Logs(view) => {
                // Whole characters only: one cut between two pieces waits for the rest
                r.body.extend(body);
                let whole = match std::str::from_utf8(&r.body) {
                    Err(e) if e.error_len().is_none() => e.valid_up_to(),
                    _ => r.body.len(),
                };
                let text: Vec<u8> = r.body.drain(..whole).collect();
                if !text.is_empty() {
                    out.push(Action::Append((view, String::from_utf8_lossy(&text).into_owned())));
                }
            }
            _ => {
                r.body.extend(body);
                if r.resp.done() {
                    self.dirty = true;
                    let r = self.reqs.remove(&id).expect("just seen");
                    out.push(Action::Stop(id));
                    out.extend(self.finished(r));
                }
            }
        }
        out
    }

    fn closed(&mut self, id: u64, why: &str) -> Vec<Action> {
        let Some(mut r) = self.reqs.remove(&id) else { return Vec::new() };
        match r.purpose {
            // A watch ends (its timeout, a lost connection)
            Purpose::Watch(sid) => self.watch_ended(sid, why),
            Purpose::Logs(view) if !why.is_empty() => vec![Action::Append((view, format!("\n[{why}]\n")))],
            Purpose::Logs(_) => Vec::new(),
            _ => match r.resp.closed() {
                Ok(()) => self.finished(r),
                Err(e) => {
                    let why = if why.is_empty() { e } else { why.to_string() };
                    self.reqs.insert(id, r);
                    self.req_failed(id, why, None)
                }
            },
        }
    }

    fn finished(&mut self, r: Req) -> Vec<Action> {
        match r.purpose {
            Purpose::Discover(av) => {
                let kinds = serde_json::from_slice::<Value>(&r.body)
                    .ok()
                    .and_then(|v| v["resources"].as_array().cloned())
                    .unwrap_or_default()
                    .iter()
                    // (not a subresource: `pods/log`)
                    .filter(|x| x["name"].as_str().is_some_and(|n| !n.contains('/')))
                    .map(|x| (status::text(&x["kind"]), status::text(&x["name"]), x["namespaced"] == true))
                    .collect();
                self.served.insert(av, Served::Kinds(kinds));
                self.follow()
            }
            Purpose::List(sid) => match serde_json::from_slice::<kube_core::ObjectList<DynamicObject>>(&r.body) {
                Ok(l) => {
                    let Some(s) = self.streams.get_mut(&sid) else { return Vec::new() };
                    s.listing.get_or_insert_with(BTreeMap::new).extend(l.items.into_iter().map(|o| (name_of(&o), o)));
                    match l.metadata.continue_.filter(|c| !c.is_empty()) {
                        // More than a page: the next
                        Some(token) => self.list_page(sid, Some(token)),
                        None => {
                            s.objects = s.listing.take().unwrap_or_default();
                            s.version = l.metadata.resource_version.unwrap_or_default();
                            (s.loaded, s.error, s.retry) = (true, None, 0);
                            // (a workload's: its pods, by its selector, asked for now)
                            if WITH_PODS.contains(&s.kind.as_str()) {
                                self.refollow = true;
                            }
                            self.watch(sid)
                        }
                    }
                }
                Err(e) => self.again(sid, format!("the list: {e}")),
            },
            Purpose::Yaml(key, title, mask) => {
                let yaml = match serde_json::from_slice::<Value>(&r.body) {
                    Ok(mut v) => {
                        if let Some(m) = v.get_mut("metadata").and_then(Value::as_object_mut) {
                            m.remove("managedFields");
                        }
                        // A Secret's values: not shown
                        if mask {
                            for field in ["data", "stringData"] {
                                if let Some(m) = v.get_mut(field).and_then(Value::as_object_mut) {
                                    for x in m.values_mut() {
                                        *x = Value::String("••••••".into());
                                    }
                                }
                            }
                        }
                        serde_yaml::to_string(&v).unwrap_or_default()
                    }
                    Err(e) => format!("# couldn't read it: {e}\n"),
                };
                let id = self.view(&key);
                vec![Action::Show(View { id, title, text: yaml, language: Some("yaml".into()) })]
            }
            Purpose::Events(key, title, name, head) => {
                let now = tarae_plugin::now::millis() as i64;
                let mine = |e: &Value| {
                    let of = status::text(&e["involvedObject"]["name"]);
                    of == name || of.starts_with(&format!("{name}-"))
                };
                let mut items: Vec<Value> = serde_json::from_slice::<Value>(&r.body)
                    .ok()
                    .and_then(|v| v["items"].as_array().cloned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(mine)
                    .collect();
                let when = |e: &Value| {
                    status::millis(&e["lastTimestamp"]).or_else(|| status::millis(&e["eventTime"])).unwrap_or(0)
                };
                items.sort_by_key(|e| std::cmp::Reverse(when(e)));
                let mut text: String = items
                    .iter()
                    .map(|e| {
                        let count =
                            e["count"].as_u64().filter(|&c| c > 1).map_or(String::new(), |c| format!(" (×{c})"));
                        format!(
                            "{:>6} ago  {:<7} {} {}{count}: {}\n",
                            status::human(now - when(e)),
                            status::text(&e["type"]),
                            status::text(&e["involvedObject"]["name"]),
                            status::text(&e["reason"]),
                            status::text(&e["message"]).trim()
                        )
                    })
                    .collect();
                if text.is_empty() {
                    text = "no events (the cluster keeps them about an hour)\n".into();
                }
                // (described: what it is, then its events)
                if !head.is_empty() {
                    text = format!("{head}\nEvents\n{text}");
                }
                let id = self.view(&key);
                vec![Action::Show(View { id, title, text, language: None })]
            }
            Purpose::Labeled(look, i, _) if look == self.labeling.0 => {
                let items = serde_json::from_slice::<Value>(&r.body)
                    .ok()
                    .and_then(|v| v["items"].as_array().cloned())
                    .unwrap_or_default();
                for o in items {
                    let m = &o["metadata"];
                    let release = m["labels"]["app.kubernetes.io/instance"]
                        .as_str()
                        .or(m["labels"]["release"].as_str());
                    let (chart, ns) =
                        (status::text(&m["labels"]["helm.sh/chart"]), status::text(&m["namespace"]));
                    self.labeling.1.push((chart, release.unwrap_or_default().to_string(), ns));
                }
                self.label(i + 1, true)
            }
            Purpose::Watch(_) | Purpose::Logs(_) | Purpose::Labeled(..) => Vec::new(),
        }
    }

    /// What tarae couldn't do for it.
    fn failed(&mut self, id: u64, why: String) -> Vec<Action> {
        match id {
            // A buffer of its closed
            id if id > FIRST_VIEW => self.view_closed(id),
            id if self.reqs.contains_key(&id) => self.req_failed(id, why, None),
            EXEC => self.exec_done(None, &[why]),
            ENV | KUBECONFIG => self.fail(why),
            id if (FILES..EXEC).contains(&id) => {
                self.loading = None;
                self.fail(why)
            }
            id if (KZ_READ..KZ_WRITE).contains(&id) => self.kz_read((id - KZ_READ) as usize, None),
            id if (KZ_WRITE..KZ_BUILD).contains(&id) => self.kz_failed((id - KZ_WRITE) as usize, why),
            id if (KZ_BUILD..HELM_RENDER).contains(&id) => self.kz_failed((id - KZ_BUILD) as usize, why),
            id if (HELM_READ..KZ_READ).contains(&id) => self.chart_read((id - HELM_READ) as usize, None),
            id if (HELM_RENDER..FIRST_VIEW).contains(&id) => {
                self.render_failed((id - HELM_RENDER) as usize, why)
            }
            FIND_CHARTS => self.found_charts(Vec::new()),
            // (a release's values not asked for — helm missing is said by its render — or not written)
            id if (VALUES_GET..VALUES_WRITE).contains(&id) => self.values_got((id - VALUES_GET) as usize, None, &[why]),
            id if (VALUES_WRITE..TPL_READ).contains(&id) => self.values_written((id - VALUES_WRITE) as usize, None),
            // (a chart's templates that couldn't be found or read: its values say only which releases)
            FIND_TEMPLATES => Vec::new(),
            id if (TPL_READ..HELM_READ).contains(&id) => Vec::new(),
            HELM_INSTALL => {
                self.no_helm = Some(why);
                self.follow()
            }
            // (the project's kustomizations can't be found — not allowed: plain files, then; its own folder,
            // an install that didn't go: nothing built)
            FIND_KZ => self.found(Vec::new()),
            OWN_DIR | KZ_INSTALL => {
                self.no_kustomize = Some(why);
                self.follow()
            }
            // (a pick it offered when it couldn't, and the like)
            _ => Vec::new(),
        }
    }

    fn answer(&mut self, id: u64, choice: Option<String>) -> Vec<Action> {
        let Some(choice) = choice else {
            self.picking = None;
            return Vec::new();
        };
        match id {
            PICK_ACTION => self.act(&choice),
            _ => Vec::new(),
        }
    }
}

// ── The plugin ─────────────────────────────────────────────────────────────

struct Plugin;

impl Guest for Plugin {
    fn commands() -> Vec<Command> {
        vec![
            Command {
                name: "kubernetes".into(),
                doc: "Kubernetes: the cluster's state beside your manifests — on, then its tab".into(),
                text: true,
            },
            Command { name: "kubernetes-off".into(), doc: "Kubernetes: off".into(), text: false },
            Command {
                name: "kubernetes-here".into(),
                doc: "Kubernetes: the object here — logs, events, live YAML".into(),
                text: false,
            },
        ]
    }

    fn subscriptions() -> Vec<Subscription> {
        // (a chart's templates are `helm`)
        let yaml = || vec!["yaml".to_string(), "helm".to_string()];
        vec![
            Subscription { event: Event::Opened, text: true, languages: yaml() },
            Subscription { event: Event::Changed, text: true, languages: yaml() },
            // (saving may change it too: a formatter)
            Subscription { event: Event::Saved, text: true, languages: yaml() },
            Subscription { event: Event::Closed, text: false, languages: yaml() },
            // (any buffer: the tab is the one in front's — a file with no objects says so; a manifest's text, to
            // read one opened before it was on)
            Subscription { event: Event::Shown, text: false, languages: Vec::new() },
            Subscription { event: Event::Shown, text: true, languages: yaml() },
        ]
    }

    fn permissions() -> Vec<Permission> {
        let mut p =
            vec![Permission::Env("KUBECONFIG".into()), Permission::Read("~/.kube/**".into()), Permission::Network];
        // Kustomize: the project's kustomizations read, a root built
        p.extend([Permission::Project, Permission::Run("kustomize".into())]);
        // Helm: a chart's release rendered
        p.push(Permission::Run("helm".into()));
        // EKS: `aws eks get-token`, with your profile and region
        p.extend(kubeconfig::EXEC_PROGRAMS.iter().map(|&x| Permission::Run(x.into())));
        p.extend(kubeconfig::EXEC_ENV.iter().map(|&x| Permission::Env(x.into())));
        p
    }

    fn run(name: String, cx: Context) -> Vec<Action> {
        tarae_plugin::now::set(cx.now.millis, &cx.now.entropy);
        STATE.with_borrow_mut(|s| match name.as_str() {
            "kubernetes-off" => s.stop(),
            // On already: its tab, with the keys
            "kubernetes" if s.on => {
                let side = s.side_now();
                s.side = Some(side.clone());
                vec![Action::Side(Some(Side { open: true, focus: true, ..side }))]
            }
            "kubernetes" => {
                let mut out = s.start();
                // The buffer in front, if it's a manifest (the others: as they're opened or changed)
                s.front = cx.buffer.path.clone();
                if let (Some(path), Some(text)) = (&cx.buffer.path, &cx.buffer.text) {
                    out.extend(s.file(path.clone(), cx.buffer.version, text));
                }
                out.extend(s.show());
                out
            }
            _ => {
                if !s.on {
                    return vec![Action::Warning("Kubernetes is off — :kubernetes turns it on".into())];
                }
                let line = cx.selections.get(cx.primary as usize).map_or(0, |sel| sel.line);
                let here = cx.buffer.path.as_ref().and_then(|p| {
                    let f = s.files.get(p)?;
                    let i = f.docs.iter().position(|d| d.lines.contains(&line))?;
                    Some((p.clone(), i))
                });
                match here {
                    Some((path, i)) => s.pick_action(path, i),
                    None => vec![Action::Status("no Kubernetes object here".into())],
                }
            }
        })
    }

    fn on(event: Event, cx: Context) -> Vec<Action> {
        tarae_plugin::now::set(cx.now.millis, &cx.now.entropy);
        if event == Event::Shown {
            let b = cx.buffer;
            return STATE.with_borrow_mut(|s| s.shown(b.path, b.version, b.text.as_deref()));
        }
        let Some(path) = cx.buffer.path.clone() else { return Vec::new() };
        STATE.with_borrow_mut(|s| match event {
            Event::Opened | Event::Changed | Event::Saved => {
                // Kustomize reads what's on disk: a kustomization, or a file one takes in, saved — built
                // again (and a kustomization opened: maybe one not seen yet)
                let rescan = s.on
                    && s.cluster.is_some()
                    && match event {
                        Event::Saved => {
                            kustomize::is_kustomization(&path)
                                || !s.tree.roots_of(&path).is_empty()
                                || path.ends_with("/Chart.yaml")
                        }
                        Event::Opened => {
                            kustomize::is_kustomization(&path) && !s.tree.kzs.contains_key(kustomize::parent(&path))
                                // (a template of a chart not seen yet)
                                || helm::is_template(&path)
                                    && s.charts.as_ref().is_some_and(|c| helm::source_of(c, &path).is_none())
                        }
                        _ => false,
                    };
                if s.on && event == Event::Saved {
                    s.chart_saved(&path);
                }
                let mut out = s.file(path, cx.buffer.version, cx.buffer.text.as_deref().unwrap_or(""));
                if rescan {
                    out.extend(s.scan());
                }
                out
            }
            Event::Closed => s.file_closed(&path),
            _ => Vec::new(),
        })
    }

    fn on_events(events: Vec<HostEvent>, now: Now) -> Vec<Action> {
        tarae_plugin::now::set(now.millis, &now.entropy);
        STATE.with_borrow_mut(|s| s.on_events(events))
    }
}

impl State {
    /// What came back from what it asked for.
    fn on_events(&mut self, events: Vec<HostEvent>) -> Vec<Action> {
        let s = self;
        if !s.on {
            return Vec::new();
        }
        let mut out = Vec::new();
        // Only bytes from the cluster: shown again only if they changed what's shown
        let bytes = events.iter().all(|e| matches!(e, HostEvent::Data(_)));
        (s.dirty, s.refollow) = (false, false);
        for ev in events {
            out.extend(match ev {
                HostEvent::Env((ENV, value)) => {
                    // KUBECONFIG may list several: the first
                    s.kubeconfig = value
                        .as_deref()
                        .and_then(|v| v.split(':').find(|p| !p.is_empty()))
                        .map_or("~/.kube/config".to_string(), str::to_string);
                    vec![Action::ReadFile((KUBECONFIG, s.kubeconfig.clone()))]
                }
                HostEvent::File((KUBECONFIG, bytes)) => s.read_kubeconfig(&bytes),
                HostEvent::File((id, bytes)) if (KZ_READ..KZ_WRITE).contains(&id) => {
                    s.kz_read((id - KZ_READ) as usize, Some(&bytes))
                }
                HostEvent::Found((FIND_KZ, paths)) => s.found(paths),
                HostEvent::Written((OWN_DIR, path)) => {
                    s.own_dir = Some(kustomize::parent(&path).to_string());
                    s.build()
                }
                HostEvent::Lines((id, lines)) if (VALUES_GET..VALUES_WRITE).contains(&id) => {
                    if let Some(Given::Asking(out)) =
                        s.render_keys.get((id - VALUES_GET) as usize).and_then(|k| s.release_values.get_mut(k))
                    {
                        out.extend(lines);
                    }
                    Vec::new()
                }
                HostEvent::Exit((id, code, err)) if (VALUES_GET..VALUES_WRITE).contains(&id) => {
                    s.values_got((id - VALUES_GET) as usize, code, &err)
                }
                HostEvent::Written((id, path)) if (VALUES_WRITE..TPL_READ).contains(&id) => {
                    s.values_written((id - VALUES_WRITE) as usize, Some(path))
                }
                HostEvent::Written((id, path)) if (KZ_WRITE..KZ_BUILD).contains(&id) => {
                    s.kz_written((id - KZ_WRITE) as usize, &path)
                }
                HostEvent::Lines((id, lines)) if (KZ_BUILD..HELM_RENDER).contains(&id) => {
                    s.kz_lines((id - KZ_BUILD) as usize, lines);
                    Vec::new()
                }
                HostEvent::Exit((id, code, err)) if (KZ_BUILD..HELM_RENDER).contains(&id) => {
                    s.kz_built((id - KZ_BUILD) as usize, code, &err)
                }
                HostEvent::File((id, bytes)) if (HELM_READ..KZ_READ).contains(&id) => {
                    s.chart_read((id - HELM_READ) as usize, Some(&bytes))
                }
                HostEvent::Found((FIND_CHARTS, paths)) => s.found_charts(paths),
                HostEvent::Found((FIND_TEMPLATES, paths)) => {
                    s.templates = paths;
                    s.read_templates()
                }
                HostEvent::File((id, bytes)) if (TPL_READ..HELM_READ).contains(&id) => {
                    s.template_read((id - TPL_READ) as usize, &bytes)
                }
                HostEvent::Lines((id, lines)) if (HELM_RENDER..FIRST_VIEW).contains(&id) => {
                    s.render_lines((id - HELM_RENDER) as usize, lines);
                    Vec::new()
                }
                HostEvent::Exit((id, code, err)) if (HELM_RENDER..FIRST_VIEW).contains(&id) => {
                    s.rendered((id - HELM_RENDER) as usize, code, &err)
                }
                HostEvent::Installed(HELM_INSTALL) => {
                    s.no_helm = None;
                    s.renders.clear();
                    s.render()
                }
                HostEvent::Installed(KZ_INSTALL) => {
                    s.no_kustomize = None;
                    s.builds.clear();
                    s.build()
                }
                HostEvent::File((id, bytes)) => s.file_read(id, bytes),
                HostEvent::Connected(id) => s.connected(id),
                HostEvent::Data((id, bytes)) => s.data(id, &bytes),
                HostEvent::Closed((id, why)) => s.closed(id, &why),
                HostEvent::Tick(REFRESH) => s.run_exec(),
                HostEvent::Tick(t) if t > RETRY && t < FIRST_VIEW => s.list(t - RETRY),
                HostEvent::Lines((EXEC, lines)) => {
                    s.exec_out.extend(lines);
                    Vec::new()
                }
                HostEvent::Exit((EXEC, code, err)) => s.exec_done(code, &err),
                HostEvent::Answer((id, choice)) => s.answer(id, choice),
                HostEvent::Key((k, _, id)) => s.side_key(&k, id),
                HostEvent::Failed((id, why)) => s.failed(id, why),
                _ => Vec::new(),
            });
        }
        if s.refollow {
            // (it shows, too)
            out.extend(s.follow());
        } else if !bytes || s.dirty {
            out.extend(s.show());
        }
        out
    }
}

export!(Plugin);

#[cfg(test)]
mod tests {
    use super::*;

    fn with_cluster() -> State {
        tarae_plugin::now::set(1_000_000, &[7; 32]);
        let config = tls::config(b"", None).unwrap();
        State {
            on: true,
            namespace: "default".into(),
            cluster: Some(Cluster {
                context: "prod-eks".into(),
                host: "k".into(),
                port: 443,
                base: String::new(),
                server_name: "k".into(),
                tls: config,
                token: None,
            }),
            ..State::default()
        }
    }

    const MANIFEST: &str = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n---\napiVersion: v1\nkind: Service\nmetadata:\n  name: web\n  namespace: prod\n";

    /// `helm get values` for release `n` says `json`.
    fn answer_values(s: &mut State, n: usize, json: &str) -> Vec<Action> {
        let key = s.render_keys[n].clone();
        if let Some(Given::Asking(out)) = s.release_values.get_mut(&key) {
            out.push(json.to_string());
        }
        s.values_got(n, Some(0), &[])
    }

    fn serve(s: &mut State, av: &str, kinds: &[(&str, &str)]) -> Vec<Action> {
        let resources: Vec<Value> =
            kinds.iter().map(|(k, p)| serde_json::json!({"kind": k, "name": p, "namespaced": true})).collect();
        let body = serde_json::json!({ "resources": resources }).to_string().into_bytes();
        let purpose = Purpose::Discover(av.into());
        s.finished(Req { purpose, http: vec![], tls: None, resp: Response::new(), lines: Lines::new(), body })
    }

    #[test]
    fn a_manifest_asks_the_cluster_about_its_kinds_then_watches_them() {
        let mut s = with_cluster();
        let out = s.file("/r/web.yaml".into(), 3, MANIFEST);
        assert!(out.iter().any(|a| matches!(a, Action::Side(Some(side)) if side.open)), "the tab opens, the first time");
        let asked = out.iter().filter(|a| matches!(a, Action::Connect(_))).count();
        assert_eq!(asked, 2, "apps/v1 and v1, once each");
        let again = s.file("/r/web.yaml".into(), 4, MANIFEST);
        assert!(again.iter().all(|a| !matches!(a, Action::Connect(_))), "not again");
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        serve(&mut s, "v1", &[("Service", "services"), ("Pod", "pods"), ("Binding", "pods/binding")]);
        let watched = |s: &State| {
            let mut w: Vec<String> =
                s.streams.values().map(|st| format!("{} {:?} {:?}", st.what.plural, st.what.ns, st.select)).collect();
            w.sort();
            w
        };
        assert_eq!(
            watched(&s),
            ["deployments Some(\"default\") Name(\"web\")", "services Some(\"prod\") Name(\"web\")"],
            "one stream per kind and namespace, of only the object named (its pods: once it's seen)"
        );
        // (the server is asked for that one only)
        let asks: Vec<String> = s.reqs.values().map(|r| String::from_utf8_lossy(&r.http).into_owned()).collect();
        assert!(asks.iter().any(|h| h.contains("/deployments?") && h.contains("fieldSelector=metadata.name%3Dweb")), "{asks:?}");
        // Seen: their pods, by their selectors
        let list = |kind: &str, sel: Value| {
            serde_json::json!({"apiVersion": "v1", "kind": "List", "metadata": {"resourceVersion": "1"},
                "items": [{"apiVersion": "v1", "kind": kind, "metadata": {"name": "web"}, "spec": {"selector": sel}}]})
        };
        for (sid, kind, sel) in [
            (s.streams.iter().find(|(_, st)| st.kind == "Deployment").map(|(i, _)| *i).unwrap(), "Deployment",
                serde_json::json!({"matchLabels": {"app": "web", "tier": "front"}})),
            (s.streams.iter().find(|(_, st)| st.kind == "Service").map(|(i, _)| *i).unwrap(), "Service",
                serde_json::json!({"matchExpressions": [{"key": "app", "operator": "Exists"}]})),
        ] {
            let body = list(kind, sel).to_string().into_bytes();
            let r = Req { purpose: Purpose::List(sid), http: vec![], tls: None, resp: Response::new(), lines: Lines::new(), body };
            s.finished(r);
        }
        assert!(s.refollow, "a workload seen: what's watched looked at again");
        s.follow();
        assert_eq!(
            watched(&s),
            [
                "deployments Some(\"default\") Name(\"web\")",
                "pods Some(\"default\") Labels(\"app=web,tier=front\")",
                // (a selector with expressions: the namespace's pods, all of them)
                "pods Some(\"prod\") All",
                "services Some(\"prod\") Name(\"web\")"
            ]
        );
        // Two of a kind in a namespace: all of the kind there (a selector can't name two)
        let two = format!("{MANIFEST}---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: api\n");
        s.file("/r/web.yaml".into(), 5, &two);
        assert!(watched(&s).contains(&"deployments Some(\"default\") All".to_string()), "{:?}", watched(&s));
        // Closed: nothing left to watch
        s.file_closed("/r/web.yaml");
        assert!(s.streams.is_empty());
    }

    /// Helm's labels are looked for across the cluster only when the project has a chart.
    #[test]
    fn a_project_without_charts_doesnt_list_the_cluster_for_releases() {
        let mut s = with_cluster();
        let lists = |s: &State| s.reqs.values().filter(|r| matches!(r.purpose, Purpose::Labeled(..))).count();
        s.found_charts(Vec::new());
        assert_eq!((lists(&s), s.labeled.as_ref().map(Vec::len)), (0, Some(0)));
        s.chart_reading = (vec!["/r/charts/shop/Chart.yaml".into()], BTreeMap::new(), 1);
        s.chart_read(0, Some(b"name: shop\n"));
        assert_eq!(lists(&s), 1, "one kind at a time");
    }

    #[test]
    fn a_change_nobody_has_open_shows_nothing_again() {
        let mut s = with_cluster();
        let two = format!("{MANIFEST}---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: api\n");
        s.file("/r/web.yaml".into(), 3, &two);
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        let sid = s.streams.iter().find(|(_, st)| st.kind == "Deployment").map(|(i, _)| *i).unwrap();
        let change = |name: &str| {
            serde_json::json!({"type": "MODIFIED", "object": {"apiVersion": "apps/v1", "kind": "Deployment",
                "metadata": {"name": name, "resourceVersion": "7"}}}).to_string()
        };
        s.dirty = false;
        s.watch_event(sid, &change("other"));
        assert!(!s.dirty, "another deployment of the namespace: nobody's");
        assert_eq!(s.streams[&sid].version, "7", "(still where the watch goes on from)");
        s.watch_event(sid, &change("api"));
        assert!(s.dirty && s.refollow, "one a manifest has: shown again, and its pods looked for");
        // A batch of only bytes that changed nothing shown: not worked out again; anything else, it is
        s.dirty = false;
        let web = serde_json::json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "api"},
            "spec": {"replicas": 1}, "status": {"readyReplicas": 1, "updatedReplicas": 1}});
        s.streams.get_mut(&sid).unwrap().loaded = true;
        s.streams.get_mut(&sid).unwrap().objects.insert("api".into(), serde_json::from_value(web).unwrap());
        let hints = |out: &[Action]| out.iter().any(|a| matches!(a, Action::Hints(_)));
        assert!(!hints(&s.on_events(vec![HostEvent::Data((FIRST_CONN + 999, b"x".to_vec()))])));
        assert!(hints(&s.on_events(vec![HostEvent::Tick(7)])));
    }

    /// The side tab is the file in front's — another open file shows only if it isn't fine, to go to; a
    /// file with no objects says so.
    #[test]
    fn the_side_tab_is_the_file_in_front() {
        let mut s = with_cluster();
        let deploy = |name: &str| format!("apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {name}\n  namespace: prod\n");
        s.file("/r/a.yaml".into(), 1, &deploy("a"));
        s.file("/r/b.yaml".into(), 1, &deploy("b"));
        s.file("/r/c.yaml".into(), 1, &deploy("c"));
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        let ok = |name: &str| serde_json::json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": name, "namespace": "prod"},
            "spec": {"replicas": 1}, "status": {"readyReplicas": 1, "updatedReplicas": 1}});
        for st in s.streams.values_mut() {
            st.loaded = true;
            // (a and c are there; b isn't)
            st.objects.insert("a".into(), serde_json::from_value(ok("a")).unwrap());
            st.objects.insert("c".into(), serde_json::from_value(ok("c")).unwrap());
        }
        let rows = |s: &State| {
            s.side_now().rows.iter().map(|r| (r.depth, r.text.clone(), r.at.as_ref().map(|a| a.0.clone()))).collect::<Vec<_>>()
        };
        s.shown(Some("/r/a.yaml".into()), 1, None);
        let got = rows(&s);
        assert_eq!(got[0], (0, "r/a.yaml".to_string(), Some("/r/a.yaml".to_string())), "{got:?}");
        assert!(!got.iter().any(|r| r.1 == "c"), "a fine file elsewhere: not here");
        assert_eq!(
            &got[got.len() - 2..],
            [(0, "other open files".to_string(), None), (1, "r/b.yaml".to_string(), Some("/r/b.yaml".to_string()))],
            "one that isn't fine: to go to"
        );
        s.shown(Some("/r/notes.md".into()), 1, None);
        assert_eq!(rows(&s)[0].1, "no objects in this file");
        s.shown(Some("/r/c.yaml".into()), 1, None);
        assert_eq!(rows(&s)[0].1, "r/c.yaml");
        // One open before it was on: read as it comes to the front
        s.shown(Some("/r/d.yaml".into()), 4, Some(&deploy("c")));
        assert_eq!((rows(&s)[0].1.as_str(), s.files["/r/d.yaml"].version), ("r/d.yaml", 4));
    }

    #[test]
    fn hints_and_the_side_tab_say_how_each_object_is_doing() {
        let mut s = with_cluster();
        s.file("/r/web.yaml".into(), 3, MANIFEST);
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        serve(&mut s, "v1", &[("Service", "services"), ("Pod", "pods")]);
        let web = serde_json::json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"},
            "spec": {"replicas": 2, "selector": {"matchLabels": {"app": "web"}}},
            "status": {"readyReplicas": 2, "updatedReplicas": 2}});
        for st in s.streams.values_mut() {
            st.loaded = true;
            if st.what.plural == "deployments" {
                st.objects.insert("web".into(), serde_json::from_value(web.clone()).unwrap());
            }
        }
        let out = s.shown(Some("/r/web.yaml".into()), 3, None);
        let hints: Vec<&Hints> = out.iter().filter_map(|a| if let Action::Hints(h) = a { Some(h) } else { None }).collect();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].version, 3, "for the text it read");
        let said: Vec<(u64, &str)> = hints[0].hints.iter().map(|h| (h.line, h.text.as_str())).collect();
        assert_eq!(said, [(1, "2/2 ready  (ns default, from the context)"), (6, "not in prod-eks")]);
        // The badge: the cluster · the side tab: the file, its objects
        assert_eq!(s.badge, Some(("⎈ prod-eks".into(), Tone::Ok)));
        let Some(Action::Side(Some(side))) = out.iter().find(|a| matches!(a, Action::Side(_))) else { panic!() };
        assert!(!side.open, "opened at it the first time only");
        let rows: Vec<(u64, &str, &str)> = side.rows.iter().map(|r| (r.depth, r.text.as_str(), r.detail.as_str())).collect();
        assert_eq!(
            rows,
            [
                (0, "r/web.yaml", "▲1"),
                (1, "Deployment", "1"),
                (2, "web", "2/2"),
                (1, "Service", "▲1"),
                (2, "web", ""),
                (3, "not in prod-eks", ""),
            ]
        );
        assert_eq!(side.rows[4].at, Some(("/r/web.yaml".into(), 5, 9)), "where it's defined");
        assert!(side.rows[5].note && !side.rows[4].note, "why it isn't fine: a line of its own");
        assert!(s.show().is_empty(), "nothing changed: nothing sent");
        // Edited (the same objects): the hints sent again, for the new version; the tab doesn't open again
        let out = s.file("/r/web.yaml".into(), 4, MANIFEST);
        assert!(out.iter().any(|a| matches!(a, Action::Hints(h) if h.version == 4 && h.hints.len() == 2)));
        // A key on its row: what to do with it
        let out = s.side_key("a", Some(side.rows[4].id.clone()));
        let Some(Action::Pick(p)) = out.first() else { panic!() };
        assert_eq!(p.items[0], ("goto".to_string(), "go to web.yaml:7".to_string()));
        assert_eq!(s.answer(PICK_ACTION, Some("goto".into())), [Action::Open("/r/web.yaml:7".into())]);
    }

    #[test]
    fn a_base_file_is_read_through_the_build_of_the_root_the_cluster_has() {
        let mut s = with_cluster();
        let base = "/r/web/base/deployment.yaml";
        let k = |path: &str, text: &str| kustomize::parse(path, text);
        let kzs = [
            k("/r/web/base/kustomization.yaml", "resources: [deployment.yaml]\n"),
            k("/r/web/prod/kustomization.yaml", "resources: [../base]\n"),
            k("/r/web/dev/kustomization.yaml", "resources: [../base]\n"),
        ];
        s.tree = kustomize::Tree { kzs: kzs.into_iter().map(|k| (k.dir.clone(), k)).collect() };
        s.file(base.into(), 1, "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n");
        // Building: nothing said yet (not the base's own name, looked for as is)
        s.builds.insert("/r/web/prod".into(), Build::Running(Vec::new()));
        assert_eq!(s.docs(base)[0].via, Via::Building);
        assert!(s.seen(&s.docs(base)[0]).is_none());
        // Built: as prod made it
        let built = |ns: &str, name: &str| Built {
            api_version: "apps/v1".into(),
            kind: "Deployment".into(),
            name: name.into(),
            namespace: Some(ns.into()),
            path: Some(base.into()),
            configured_in: None,
        };
        s.builds.insert("/r/web/prod".into(), Build::Done(vec![built("prod", "prod-web")]));
        s.builds.insert("/r/web/dev".into(), Build::Done(vec![built("dev", "dev-web")]));
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        let watched: Vec<String> = s.watch_docs().iter().map(|d| format!("{}/{}", d.namespace.clone().unwrap(), d.name)).collect();
        assert_eq!(watched, ["dev/dev-web", "prod/prod-web"], "both roots' readings watched");
        // The cluster has prod's: that reading
        let web = serde_json::json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "prod-web", "namespace": "prod"},
            "spec": {"replicas": 1}, "status": {"readyReplicas": 1, "updatedReplicas": 1}});
        for st in s.streams.values_mut() {
            st.loaded = true;
            if st.what.ns.as_deref() == Some("prod") && st.what.plural == "deployments" {
                st.objects.insert("prod-web".into(), serde_json::from_value(web.clone()).unwrap());
            }
        }
        let d = &s.docs(base)[0];
        assert_eq!((d.name.as_str(), &d.via), ("prod-web", &Via::Built("prod".into())));
        assert_eq!(s.seen(d).unwrap().text, "1/1 ready  (via prod)");
        // kustomize missing: said, not guessed
        s.builds.clear();
        s.no_kustomize = Some("kustomize isn't installed".into());
        assert_eq!(s.seen(&s.docs(base)[0]).unwrap().text, "kustomize isn't installed");
    }

    #[test]
    fn a_chart_s_template_is_read_through_the_render_of_each_release_the_cluster_has() {
        let mut s = with_cluster();
        let tpl = "/r/charts/shop/templates/deployment.yaml";
        let text = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {{ include \"shop.fullname\" . }}\nspec:\n  {{- if not .Values.autoscaling.enabled }}\n  replicas: {{ .Values.replicaCount }}\n  {{- end }}\n  template:\n    spec:\n      containers:\n        - name: {{ .Chart.Name }}\n          imagePullPolicy: IfNotPresent\n";
        s.file(tpl.into(), 1, text);
        // Before the charts are known: nothing said (not the file's name, looked for as is)
        assert_eq!(s.docs(tpl)[0].via, Via::Building);
        s.charts = Some([("/r/charts/shop".to_string(), "shop".to_string())].into());
        s.labeled = Some(Vec::new());
        assert_eq!(s.seen(&s.docs(tpl)[0]).unwrap().text, "no release of shop in prod-eks");
        // A release: rendered with helm
        s.labeled = Some(vec![
            ("shop-0.1.0".into(), "shop-helm".into(), "shop".into()),
            ("other-1.0.0".into(), "x".into(), "y".into()),
        ]);
        let spawned = |out: &[Action]| -> Vec<Vec<String>> {
            out.iter().filter_map(|a| if let Action::Spawn(sp) = a { Some(sp.args.clone()) } else { None }).collect()
        };
        // What the release was given, first; then rendered with it
        let out = s.render();
        assert_eq!(spawned(&out), [["get", "values", "shop-helm", "--namespace", "shop", "--output", "json"]]);
        assert!(s.render().iter().all(|a| !matches!(a, Action::Spawn(_))), "once");
        let out = answer_values(&mut s, 0, "null");
        assert_eq!(spawned(&out), [["template", "shop-helm", "/r/charts/shop", "--namespace", "shop"]], "given nothing");
        assert!(s.render().iter().all(|a| !matches!(a, Action::Spawn(_))), "once");
        let output = "---\n# Source: shop/templates/deployment.yaml\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: shop-helm\n";
        s.render_lines(0, output.lines().map(str::to_string).collect());
        s.rendered(0, Some(0), &[]);
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        serve(&mut s, "v1", &[("Pod", "pods")]);
        let live = serde_json::json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "shop-helm", "namespace": "shop"},
            "spec": {"replicas": 2, "template": {"spec": {"containers": [{"name": "shop", "imagePullPolicy": "Always"}]}}},
            "status": {"readyReplicas": 2, "updatedReplicas": 2}});
        for st in s.streams.values_mut() {
            st.loaded = true;
            if st.what.plural == "deployments" {
                assert_eq!(st.what.ns.as_deref(), Some("shop"), "in the release's namespace");
                st.objects.insert("shop-helm".into(), serde_json::from_value(live.clone()).unwrap());
            }
        }
        let d = s.docs(tpl).remove(0);
        assert_eq!((d.name.as_str(), &d.via), ("shop-helm", &Via::Built("shop-helm".into())));
        assert_eq!(s.seen(&d).unwrap().text, "2/2 ready  (via shop-helm)");
        // A template its values don't render: said so — with the values it was given (none), not the chart's
        let ingress = "/r/charts/shop/templates/ingress.yaml";
        s.file(ingress.into(), 1, "{{- if .Values.ingress.enabled }}\napiVersion: networking.k8s.io/v1\nkind: Ingress\nmetadata:\n  name: x\n{{- end }}\n");
        assert_eq!(s.seen(&s.docs(ingress)[0]).unwrap().text, "shop-helm doesn't render it");
        // A template's value: what it made; a written one: where the cluster says otherwise
        let said: Vec<(u64, String)> = s.differ(&d).into_iter().map(|h| (h.line, h.text)).collect();
        assert_eq!(
            said,
            [
                (3, "= shop-helm".to_string()),
                (6, "= 2".to_string()),
                (11, "= shop".to_string()),
                (12, "→ Always".to_string())
            ]
        );
        // Its values.yaml: which releases — and beside a value a template writes as it is, what the cluster has
        // where it isn't the value; one written into more (`image.tag` into two) or worked out isn't said
        s.file("/r/charts/shop/values.yaml".into(), 1, "# shop\nreplicaCount: 1\npullPolicy: Always\n");
        let said: Vec<(u64, String)> =
            s.values_hints("/r/charts/shop/values.yaml").into_iter().map(|h| (h.line, h.text)).collect();
        assert_eq!(said, [(0, "release shop-helm · ns shop".to_string()), (1, "→ 2".to_string())]);
        // helm missing: offered once, and said
        s.renders.clear();
        let out = s.render();
        let n = out
            .iter()
            .find_map(|a| if let Action::Spawn(sp) = a { Some(sp.id - HELM_RENDER) } else { None })
            .unwrap();
        let out = s.render_failed(n as usize, "helm isn't on PATH".into());
        assert!(out.iter().any(|a| matches!(a, Action::OfferInstall((HELM_INSTALL, p)) if p == "helm")));
        assert_eq!(s.seen(&s.docs(tpl)[0]).unwrap().text, "helm isn't installed");
    }

    /// Why it can't show the cluster is said whole: once as a warning (into tarae's log), and in its tab a
    /// line at a time.
    #[test]
    fn why_it_can_t_connect_is_said_whole() {
        let mut s = State { on: true, kubeconfig: "~/.kube/config".into(), ..State::default() };
        let config = "apiVersion: v1\ncurrent-context: c\ncontexts:\n- name: c\n  context: {cluster: k, user: u}\nclusters:\n- name: k\n  cluster: {server: https://k:443}\nusers:\n- name: u\n  user:\n    exec:\n      apiVersion: client.authentication.k8s.io/v1beta1\n      command: kubelogin\n      args: [get-token]\n";
        let warned = |out: &[Action]| {
            out.iter().filter_map(|a| if let Action::Warning(w) = a { Some(w.clone()) } else { None }).collect::<Vec<_>>()
        };
        let out = s.read_kubeconfig(config.as_bytes());
        assert_eq!(
            warned(&out),
            ["Kubernetes: exec credentials with kubelogin: only aws (EKS) for now"]
        );
        assert!(warned(&s.read_kubeconfig(config.as_bytes())).is_empty(), "the same again: not said again");
        let side = s.side_now();
        let notes: Vec<&str> = side.rows.iter().filter(|r| r.note).map(|r| r.text.as_str()).collect();
        assert_eq!(notes.join(" "), s.error.clone().unwrap(), "all of it");
        assert!(notes.len() > 1 && notes.iter().all(|n| n.chars().count() <= 30), "{notes:?}");
        assert_eq!(wrapped("a bb ccc", 4), ["a bb", "ccc"]);
    }

    #[test]
    fn one_hint_a_line_saying_all_of_it_in_the_worst_tone() {
        let h = |line, text: &str, tone| Hint { line, text: text.into(), tone };
        let got = together(vec![h(3, "→ 2", Tone::Info), h(5, "ok", Tone::Ok), h(3, "OOMKilled ↻1", Tone::Error)]);
        assert_eq!(got, [h(3, "→ 2  ·  OOMKilled ↻1", Tone::Error), h(5, "ok", Tone::Ok)]);
    }

    #[test]
    fn why_its_pods_aren_t_fine_on_the_line_that_s_why() {
        let mut s = with_cluster();
        let text = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: worker\n  namespace: prod\nspec:\n  template:\n    spec:\n      containers:\n        - name: worker\n          image: nginx:no-such-tag\n";
        s.file("/r/worker.yaml".into(), 1, text);
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        serve(&mut s, "v1", &[("Pod", "pods")]);
        let live = serde_json::json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "worker", "namespace": "prod"},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "worker"}},
                "template": {"spec": {"containers": [{"name": "worker", "image": "nginx:no-such-tag-2"}]}}},
            "status": {"replicas": 1, "updatedReplicas": 1}});
        let pod = serde_json::json!({"apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": "worker-1", "namespace": "prod", "labels": {"app": "worker"}},
            "spec": {"containers": [{"name": "worker"}]},
            "status": {"phase": "Pending", "containerStatuses": [{"name": "worker", "ready": false, "restartCount": 0,
                "state": {"waiting": {"reason": "ErrImagePull", "message": "rpc error: nginx:no-such-tag-2: not found"}}}]}});
        let sid = s.streams.iter().find(|(_, st)| st.kind == "Deployment").map(|(i, _)| *i).unwrap();
        s.streams.get_mut(&sid).unwrap().loaded = true;
        s.streams.get_mut(&sid).unwrap().objects.insert("worker".into(), serde_json::from_value(live).unwrap());
        s.follow();
        let pods = s.streams.iter().find(|(_, st)| st.kind == "Pod").map(|(i, _)| *i).expect("its pods, by its selector");
        s.streams.get_mut(&pods).unwrap().loaded = true;
        s.streams.get_mut(&pods).unwrap().objects.insert("worker-1".into(), serde_json::from_value(pod).unwrap());
        s.sent.clear();
        let out = s.show();
        let hints: Vec<(u64, String, Tone)> = out
            .iter()
            .find_map(|a| if let Action::Hints(h) = a { Some(h.hints.iter().map(|h| (h.line, h.text.clone(), h.tone)).collect()) } else { None })
            .unwrap();
        // The cause and what the cluster has otherwise, on the image's line, as one hint — the worse tone
        let image: Vec<&(u64, String, Tone)> = hints.iter().filter(|h| h.0 == 10).collect();
        assert_eq!(image, [&(10, "ErrImagePull: not found  ·  → nginx:no-such-tag-2".to_string(), Tone::Error)], "{hints:?}");
    }

    /// A line that names an object the cluster hasn't says so — a key its ConfigMap hasn't, too; one an open
    /// manifest has isn't applied yet; a Service's selector no pod has.
    #[test]
    fn what_a_line_names_that_isn_t_there() {
        let mut s = with_cluster();
        let text = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: app
  namespace: prod
spec:
  template:
    spec:
      serviceAccountName: worker
      containers:
        - name: app
          env:
            - name: MODE
              valueFrom:
                configMapKeyRef: {name: app-settings, key: mode}
            - name: TOKEN
              valueFrom:
                secretKeyRef: {name: app-token, key: token}
      volumes:
        - name: data
          persistentVolumeClaim:
            claimName: data
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: data
  namespace: prod
---
apiVersion: v1
kind: Service
metadata:
  name: app
  namespace: prod
spec:
  selector:
    app: ap
";
        s.file("/r/app.yaml".into(), 1, text);
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        let v1 = [("ConfigMap", "configmaps"), ("Secret", "secrets"), ("ServiceAccount", "serviceaccounts"),
            ("PersistentVolumeClaim", "persistentvolumeclaims"), ("Service", "services"), ("Pod", "pods")];
        serve(&mut s, "v1", &v1);
        let obj = |kind: &str, name: &str, extra: Value| {
            let mut o = serde_json::json!({"apiVersion": "v1", "kind": kind, "metadata": {"name": name, "namespace": "prod"}});
            o.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            serde_json::from_value::<DynamicObject>(o).unwrap()
        };
        let have = [
            obj("ConfigMap", "app-settings", serde_json::json!({"data": {"level": "1"}})),
            obj("ServiceAccount", "worker", serde_json::json!({})),
            obj("Service", "app", serde_json::json!({"spec": {"selector": {"app": "ap"}}})),
        ];
        s.follow();
        for st in s.streams.values_mut() {
            st.loaded = true;
            for o in &have {
                if o.types.as_ref().is_some_and(|t| t.kind == st.kind) {
                    st.objects.insert(name_of(o), o.clone());
                }
            }
        }
        s.follow();
        for st in s.streams.values_mut() {
            st.loaded = true;
        }
        let hints = |s: &mut State| {
            s.sent.clear();
            let out = s.show();
            out.iter()
                .find_map(|a| if let Action::Hints(h) = a { Some(h.hints.iter().map(|h| (h.line, h.text.clone())).collect::<Vec<_>>()) } else { None })
                .unwrap()
        };
        let got = hints(&mut s);
        let at = |line: u64| got.iter().find(|h| h.0 == line).map(|h| h.1.as_str());
        assert_eq!(at(8), None, "its ServiceAccount is there");
        assert_eq!(at(14), Some("no key mode in app-settings"));
        assert_eq!(at(17), Some("no Secret app-token in prod"));
        assert_eq!(at(21), Some("PersistentVolumeClaim data isn't applied yet"), "an open manifest has it");
        assert_eq!(at(35), Some("selects no pods"));
        // A line that already says why its pods aren't fine: not said again
        let d = s.docs("/r/app.yaml").remove(0);
        assert!(s.missing(&d, &[17]).iter().all(|h| h.line != 17));
        // Through Kustomize: the name the build gave it (a prefix, a generator's hash)
        let built = |name: &str| Built {
            api_version: "v1".into(),
            kind: "ConfigMap".into(),
            name: name.into(),
            namespace: Some("prod".into()),
            path: None,
            configured_in: Some("/r/k/base/kustomization.yaml".into()),
        };
        s.builds.insert("/r/k/prod".into(), Build::Done(vec![built("prod-app-settings-5h7k"), built("other")]));
        let d = Doc { via: Via::Built("prod".into()), ..d };
        let names: Vec<String> = s.named(&d).into_iter().map(|(_, n)| n.name).collect();
        assert_eq!(names, ["worker", "prod-app-settings-5h7k", "app-token", "data"]);
    }

    /// A chart's values.yaml open on its own: its templates are found and read for where each value goes, and
    /// what each release has there is said — whose, when there are more, and who set it.
    #[test]
    fn a_value_is_followed_to_what_each_release_has() {
        let mut s = with_cluster();
        s.charts = Some([("/r/charts/shop".to_string(), "shop".to_string()), ("/r/charts/db".to_string(), "db".to_string())].into());
        s.labeled = Some(vec![
            ("shop-0.1.0".into(), "shop-a".into(), "a".into()),
            ("shop-0.1.0".into(), "shop-b".into(), "b".into()),
        ]);
        s.templates = vec![
            "/r/charts/shop/templates/deployment.yaml".into(),
            "/r/charts/db/templates/sts.yaml".into(),
        ];
        let out = s.file("/r/charts/shop/values.yaml".into(), 1, "replicaCount: 1\n");
        let reads: Vec<&str> =
            out.iter().filter_map(|a| if let Action::ReadFile((_, p)) = a { Some(p.as_str()) } else { None }).collect();
        assert_eq!(reads, ["/r/charts/shop/templates/deployment.yaml"], "its own chart's only");
        let asked = out.iter().filter(|a| matches!(a, Action::Spawn(sp) if sp.args[0] == "get")).count();
        assert_eq!(asked, 2, "each release's values asked for");
        // shop-a was given nothing; shop-b, its replicas: written for its render, which uses them
        s.own_dir = Some("/own".into());
        answer_values(&mut s, 0, "null");
        let out = answer_values(&mut s, 1, "{\"replicaCount\": 3}");
        assert!(out.iter().any(|a| matches!(a, Action::WriteOwn((_, p, _)) if p == "values/1.json")));
        let out = s.values_written(1, Some("/own/values/1.json".into()));
        let args = out.iter().find_map(|a| if let Action::Spawn(sp) = a { Some(sp.args.clone()) } else { None }).unwrap();
        assert_eq!(&args[args.len() - 2..], ["--values", "/own/values/1.json"]);
        let wiped = |out: &[Action]| out.iter().any(|a| matches!(a, Action::WriteOwn((_, p, b)) if p == "values/1.json" && b == b"{}"));
        let tpl = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {{ .Release.Name }}\nspec:\n  replicas: {{ .Values.replicaCount }}\n";
        s.template_read(0, tpl.as_bytes());
        for (n, rel) in ["shop-a", "shop-b"].iter().enumerate() {
            let output = format!("---\n# Source: shop/templates/deployment.yaml\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {rel}\n");
            s.render_lines(n, output.lines().map(str::to_string).collect());
            let out = s.rendered(n, Some(0), &[]);
            // (shop-b's values, read by its render: their file emptied)
            assert_eq!(wiped(&out), n == 1);
        }
        // Rendered again (helm installed, say): its values written again first
        s.renders.clear();
        let out = s.render();
        assert!(out.iter().any(|a| matches!(a, Action::WriteOwn((_, p, b)) if p == "values/1.json" && b != b"{}")));
        s.values_written(1, Some("/own/values/1.json".into()));
        let out = s.render();
        assert!(out.iter().all(|a| !matches!(a, Action::Spawn(sp) if sp.args.contains(&"shop-b".to_string()))), "rendering");
        for (n, rel) in ["shop-a", "shop-b"].iter().enumerate() {
            let output = format!("---\n# Source: shop/templates/deployment.yaml\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: {rel}\n");
            s.render_lines(n, output.lines().map(str::to_string).collect());
            s.rendered(n, Some(0), &[]);
        }
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        let live = |rel: &str, ns: &str, replicas: u64, by: &str| {
            serde_json::from_value::<DynamicObject>(serde_json::json!({"apiVersion": "apps/v1", "kind": "Deployment",
                "metadata": {"name": rel, "namespace": ns, "managedFields": [{"manager": by, "operation": "Update",
                    "time": "2026-10-01T00:00:00Z", "fieldsType": "FieldsV1", "fieldsV1": {"f:spec": {"f:replicas": {}}}}]},
                "spec": {"replicas": replicas}})).unwrap()
        };
        for st in s.streams.values_mut() {
            st.loaded = true;
            match st.what.ns.as_deref() {
                Some("a") => st.objects.insert("shop-a".into(), live("shop-a", "a", 1, "helm")),
                Some("b") => st.objects.insert("shop-b".into(), live("shop-b", "b", 3, "kubectl")),
                _ => None,
            };
        }
        let said: Vec<(u64, String)> =
            s.values_hints("/r/charts/shop/values.yaml").into_iter().map(|h| (h.line, h.text)).collect();
        assert_eq!(said[1], (0, "→ 3 · release values".to_string()), "only where it isn't the value; whose: its values'");
        // (on its first line with which releases: one hint, both said)
        let out = s.show();
        let first = out.iter().find_map(|a| match a {
            Action::Hints(h) if h.path.ends_with("values.yaml") => h.hints.first().map(|h| h.text.clone()),
            _ => None,
        });
        assert_eq!(first.as_deref(), Some("releases shop-a (ns a), shop-b (ns b)  ·  → 3 · release values"));
        s.streams.values_mut().filter(|st| st.what.ns.as_deref() == Some("a")).for_each(|st| {
            st.objects.insert("shop-a".into(), live("shop-a", "a", 2, "helm"));
        });
        let said: Vec<String> = s.values_hints("/r/charts/shop/values.yaml").into_iter().map(|h| h.text).collect();
        assert_eq!(said[1], "→ 2 · helm (shop-a), 3 · release values (shop-b)", "two: whose each is");
        // A chart's file saved: rendered again, and what it was given asked again
        s.chart_saved("/r/charts/shop/values.yaml");
        assert!(s.renders.is_empty() && s.release_values.is_empty());
        // Not known (no access to the release's Secret): rendered with the chart's own, and said so
        s.render();
        let n = s.render_keys.iter().position(|(_, r)| r.name == "shop-a").unwrap();
        let out = s.values_got(n, Some(1), &["Error: secrets is forbidden".into()]);
        let args = out.iter().find_map(|a| if let Action::Spawn(sp) = a { Some(sp.args.clone()) } else { None }).unwrap();
        assert!(!args.contains(&"--values".to_string()), "{args:?}");
        assert_eq!(
            s.release_values.values().find(|g| matches!(g, Given::Unknown(_))),
            Some(&Given::Unknown("helm get values: Error: secrets is forbidden".into()))
        );
    }

    #[test]
    fn the_cluster_s_value_beside_a_line_that_says_otherwise() {
        let mut s = with_cluster();
        let text = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n  namespace: prod\nspec:\n  replicas: 1\n  template:\n    spec:\n      containers:\n        - name: web\n          image: nginx:1.27\n          resources:\n            limits: {cpu: \"0.5\"}\n";
        s.file("/r/web.yaml".into(), 1, text);
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        serve(&mut s, "v1", &[("Pod", "pods")]);
        let live = serde_json::json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "namespace": "prod"},
            "spec": {"replicas": 3, "template": {"spec": {"containers": [{"name": "web", "image": "nginx:1.28", "resources": {"limits": {"cpu": "500m"}}}]}}},
            "status": {"readyReplicas": 3, "updatedReplicas": 3}});
        for st in s.streams.values_mut() {
            st.loaded = true;
            if st.what.plural == "deployments" {
                st.objects.insert("web".into(), serde_json::from_value(live.clone()).unwrap());
            }
        }
        let d = s.docs("/r/web.yaml").remove(0);
        let said: Vec<(u64, String)> = s.differ(&d).into_iter().map(|h| (h.line, h.text)).collect();
        // replicas and the image differ; the name, the namespace and the cpu (500m is 0.5) don't
        assert_eq!(said, [(6, "→ 3".to_string()), (11, "→ nginx:1.28".to_string())]);
    }

    #[test]
    fn a_watch_that_ends_goes_on_relists_on_410_and_backs_off_when_it_keeps_failing() {
        let mut s = with_cluster();
        s.file("/r/web.yaml".into(), 3, MANIFEST);
        serve(&mut s, "apps/v1", &[("Deployment", "deployments")]);
        let sid = *s.streams.keys().next().unwrap();
        let timers = |out: &[Action]| {
            out.iter()
                .filter_map(|a| if let Action::Timer((id, ms)) = a { (*id == RETRY + sid).then_some(*ms) } else { None })
                .collect::<Vec<_>>()
        };
        s.streams.get_mut(&sid).unwrap().watch_started = 1_000_000 - 60_000;
        assert!(timers(&s.watch_ended(sid, "")).is_empty(), "went on at once");
        s.streams.get_mut(&sid).unwrap().watch_started = 1_000_000 - 60_000;
        s.watch_event(
            sid,
            r#"{"type":"ERROR","object":{"kind":"Status","apiVersion":"v1","metadata":{},"status":"Failure","message":"too old resource version","reason":"Expired","code":410}}"#,
        );
        assert!(s.streams[&sid].relist);
        s.watch_ended(sid, "");
        assert!(!s.streams[&sid].relist && s.streams[&sid].listing.is_some(), "listing afresh");
        s.streams.get_mut(&sid).unwrap().watch_started = 1_000_000;
        let pauses: Vec<u64> = (0..7).flat_map(|_| timers(&s.watch_ended(sid, ""))).collect();
        assert_eq!(pauses, [1000, 2000, 4000, 8000, 16000, 30000, 30000]);
    }
}
