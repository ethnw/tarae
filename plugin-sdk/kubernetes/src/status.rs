//! What an object's state says, in a few words — as kubectl would put it, and how it reads: fine, of note,
//! a warning, an error.

use kube_core::DynamicObject;
use serde_json::Value;

use crate::tarae::plugin::types::Tone;

/// A duration as kubectl says it (`HumanDuration`): `90s`, `5m12s`, `47m`, `3h25m`, `20h`, `3d4h`, `12d`,
/// `2y30d`.
pub fn human(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (m, h) = (s / 60, s / 3600);
    let (d, y) = (h / 24, h / (24 * 365));
    let two = |big: i64, a: &str, small: i64, b: &str| {
        if small == 0 { format!("{big}{a}") } else { format!("{big}{a}{small}{b}") }
    };
    match () {
        _ if s < 120 => format!("{s}s"),
        _ if m < 10 => two(m, "m", s % 60, "s"),
        _ if m < 180 => format!("{m}m"),
        _ if h < 8 => two(h, "h", m % 60, "m"),
        _ if h < 48 => format!("{h}h"),
        _ if h < 24 * 8 => two(d, "d", h % 24, "h"),
        _ if h < 24 * 365 * 2 => format!("{d}d"),
        _ if h < 24 * 365 * 8 => two(y, "y", d % 365, "d"),
        _ => format!("{y}y"),
    }
}

/// An RFC 3339 time (as the API writes them), in milliseconds.
pub fn millis(v: &Value) -> Option<i64> {
    serde_json::from_value::<k8s_openapi::apimachinery::pkg::apis::meta::v1::Time>(v.clone())
        .ok()
        .map(|t| t.0.as_millisecond())
}

pub fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// How a container ended: its reason, else the signal or the exit code.
fn ended(term: &Value) -> String {
    match (term["reason"].as_str().filter(|r| !r.is_empty()), term["signal"].as_i64().filter(|&s| s != 0)) {
        (Some(r), _) => r.to_string(),
        (None, Some(sig)) => format!("Signal:{sig}"),
        (None, None) => format!("ExitCode:{}", term["exitCode"].as_i64().unwrap_or(0)),
    }
}

/// A pod's STATUS, worked out as kubectl does: init containers first, then what its containers wait on or
/// ended with, else the phase — or Terminating.
pub fn pod_status(o: &DynamicObject) -> String {
    let (d, st) = (&o.data, &o.data["status"]);
    let mut reason = st["reason"].as_str().or(st["phase"].as_str()).unwrap_or("").to_string();
    let specs = d["spec"]["initContainers"].as_array().cloned().unwrap_or_default();
    let mut initializing = false;
    for (i, c) in st["initContainerStatuses"].as_array().into_iter().flatten().enumerate() {
        let (term, wait) = (&c["state"]["terminated"], &c["state"]["waiting"]);
        let sidecar = specs.get(i).is_some_and(|s| s["restartPolicy"] == "Always");
        if term.is_object() && term["exitCode"].as_i64() == Some(0) || sidecar && c["started"] == true {
            continue;
        }
        reason = if term.is_object() {
            format!("Init:{}", ended(term))
        } else if let Some(r) = wait["reason"].as_str().filter(|r| !r.is_empty() && *r != "PodInitializing") {
            format!("Init:{r}")
        } else {
            format!("Init:{i}/{}", specs.len())
        };
        initializing = true;
        break;
    }
    if !initializing {
        let mut running = false;
        for c in st["containerStatuses"].as_array().into_iter().flatten().rev() {
            let (term, wait) = (&c["state"]["terminated"], &c["state"]["waiting"]);
            if let Some(r) = wait["reason"].as_str().filter(|r| !r.is_empty()) {
                reason = r.to_string();
            } else if term.is_object() {
                reason = ended(term);
            } else if c["ready"] == true && c["state"]["running"].is_object() {
                running = true;
            }
        }
        if reason == "Completed" && running {
            reason = if ready(st) { "Running" } else { "NotReady" }.into();
        }
    }
    let phase = st["phase"].as_str().unwrap_or("");
    if o.metadata.deletion_timestamp.is_some() {
        if st["reason"] == "NodeLost" {
            reason = "Unknown".into();
        } else if !matches!(phase, "Succeeded" | "Failed") {
            reason = "Terminating".into();
        }
    }
    reason
}

/// Whether a status's `Ready` condition is true.
fn ready(st: &Value) -> bool {
    condition(st, "Ready").is_some_and(|c| c["status"] == "True")
}

fn condition<'a>(st: &'a Value, kind: &str) -> Option<&'a Value> {
    st["conditions"].as_array()?.iter().find(|c| c["type"] == kind)
}

/// A pod status that means something is wrong, not just on its way.
fn broken(status: &str) -> bool {
    matches!(
        status,
        "CrashLoopBackOff"
            | "Error"
            | "ImagePullBackOff"
            | "ErrImagePull"
            | "InvalidImageName"
            | "CreateContainerConfigError"
            | "CreateContainerError"
            | "RunContainerError"
            | "OOMKilled"
            | "Evicted"
            | "Failed"
            | "Unknown"
    ) || status.starts_with("Init:") && !status.contains('/')
        || status.starts_with("ExitCode:")
        || status.starts_with("Signal:")
}

/// How a pod reads.
pub fn pod_tone(o: &DynamicObject) -> (String, Tone) {
    let status = pod_status(o);
    let tone = match status.as_str() {
        s if broken(s) => Tone::Error,
        "Completed" | "Succeeded" => Tone::Ok,
        "Running" if ready(&o.data["status"]) => Tone::Ok,
        _ => Tone::Warning,
    };
    (status, tone)
}

/// Whether a pod hasn't ended: not `Succeeded` or `Failed` (evicted, shut down with its node, done). An ended
/// pod stays until it's collected and says what happened then, not how its workload is now.
pub fn live(p: &DynamicObject) -> bool {
    !matches!(p.data["status"]["phase"].as_str(), Some("Succeeded" | "Failed"))
}

/// Whether a pod takes a Service's traffic: running and ready.
fn serves(p: &DynamicObject) -> bool {
    p.data["status"]["phase"] == "Running" && ready(&p.data["status"])
}

/// Whether `pod` is one `selector` (a label selector, or a service's plain map) picks. A selector with
/// expressions picks none (not worked out here).
pub fn selects(selector: &Value, pod: &DynamicObject) -> bool {
    let labels = pod.metadata.labels.as_ref();
    let map = selector.get("matchLabels").unwrap_or(selector);
    if selector.get("matchExpressions").is_some_and(|e| e.as_array().is_some_and(|a| !a.is_empty())) {
        return false;
    }
    let Some(map) = map.as_object().filter(|m| !m.is_empty()) else { return false };
    map.iter().all(|(k, v)| labels.and_then(|l| l.get(k)).is_some_and(|l| Some(l.as_str()) == v.as_str()))
}

/// A selector as the API takes it (`app=web,tier=front`) — none if it has expressions or is empty (`selects`
/// doesn't read those either).
pub fn label_selector(selector: &Value) -> Option<String> {
    if selector.get("matchExpressions").is_some_and(|e| e.as_array().is_some_and(|a| !a.is_empty())) {
        return None;
    }
    let map = selector.get("matchLabels").unwrap_or(selector).as_object().filter(|m| !m.is_empty())?;
    let each: Option<Vec<String>> = map.iter().map(|(k, v)| v.as_str().map(|v| format!("{k}={v}"))).collect();
    Some(each?.join(","))
}

/// The worst of these pods' states, if any is wrong: (the status, how many pods have it).
fn worst(pods: &[&DynamicObject]) -> Option<(String, Tone, usize)> {
    let states: Vec<(String, Tone)> = pods.iter().map(|p| pod_tone(p)).filter(|(_, t)| *t != Tone::Ok).collect();
    let (s, t) = states.iter().max_by_key(|(_, t)| matches!(t, Tone::Error))?.clone();
    let n = states.iter().filter(|(x, _)| *x == s).count();
    Some((s, t, n))
}

fn n(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

/// `2/3 ready` for a workload that wants `want` and has `ready`, with what's wrong among its pods — and,
/// last, their restarts.
fn replicas(ready: u64, want: u64, updated: Option<u64>, pods: &[&DynamicObject]) -> (String, Tone) {
    if want == 0 {
        return ("scaled to 0".into(), Tone::Info);
    }
    let base = format!("{ready}/{want} ready");
    let (text, tone) = match worst(pods) {
        Some((s, Tone::Error, k)) => (format!("{base} · {k} {s}"), Tone::Error),
        _ if ready >= want && updated.is_none_or(|u| u >= want) => (base, Tone::Ok),
        _ if updated.is_some_and(|u| u < want) => (format!("{base} · rolling out"), Tone::Warning),
        Some((s, _, k)) => (format!("{base} · {k} {s}"), Tone::Warning),
        None => (base, Tone::Warning),
    };
    match pods.iter().map(|p| pod_restarts(p)).sum() {
        0 => (text, tone),
        n => (format!("{text} · {}", restarts(n)), tone),
    }
}

/// What an object's state says, by its kind — `pods` the pods of its namespace (for workloads and
/// services).
pub fn summary(kind: &str, o: &DynamicObject, pods: &[&DynamicObject]) -> (String, Tone) {
    let (text, tone) = by_kind(kind, o, pods);
    // What kstatus says goes with it, when it's worse (not observed yet, reconciling, stalled, a rollout
    // past its deadline, being deleted)
    match kstatus(kind, o) {
        (Tone::Ok, _) => (text, tone),
        (t, why) if severity(t) > severity(tone) => {
            let text = if text.is_empty() || text == "exists" || text == "Ready" { why } else { format!("{text} · {why}") };
            (text, t)
        }
        _ => (text, tone),
    }
}

fn severity(t: Tone) -> u8 {
    match t {
        Tone::Ok => 0,
        Tone::Info => 1,
        Tone::Warning => 2,
        Tone::Error => 3,
    }
}

/// kstatus (sigs.k8s.io/cli-utils — what Flux and kpt wait on): whether an object is current, in progress,
/// failed or terminating, from what every object says — its deletion, whether its controller has seen its
/// latest generation, its `Reconciling`, `Stalled` and `Ready` conditions — and a Deployment's progress
/// deadline. (Ok: current; Warning: in progress or terminating; Error: failed.)
pub fn kstatus(kind: &str, o: &DynamicObject) -> (Tone, String) {
    let st = &o.data["status"];
    if o.metadata.deletion_timestamp.is_some() {
        return (Tone::Warning, "Terminating".into());
    }
    let is = |c: &str| condition(st, c).is_some_and(|c| c["status"] == "True");
    let said = |c: &str, or: &str| {
        condition(st, c)
            .map(|c| [text(&c["reason"]), text(&c["message"])].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(": "))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| or.to_string())
    };
    if is("Stalled") {
        return (Tone::Error, said("Stalled", "stalled"));
    }
    if kind == "Deployment" && condition(st, "Progressing").is_some_and(|c| c["reason"] == "ProgressDeadlineExceeded") {
        return (Tone::Error, "ProgressDeadlineExceeded".into());
    }
    if let (Some(generation), Some(seen)) = (o.metadata.generation, st["observedGeneration"].as_i64())
        && seen < generation
    {
        return (Tone::Warning, format!("generation {generation} not observed yet"));
    }
    if is("Reconciling") {
        return (Tone::Warning, said("Reconciling", "reconciling"));
    }
    match condition(st, "Ready") {
        Some(c) if c["status"] == "False" => (Tone::Warning, said("Ready", "not ready")),
        Some(c) if c["status"] != "True" => (Tone::Warning, said("Ready", "not ready yet")),
        _ => (Tone::Ok, String::new()),
    }
}

fn by_kind(kind: &str, o: &DynamicObject, pods: &[&DynamicObject]) -> (String, Tone) {
    let (spec, st) = (&o.data["spec"], &o.data["status"]);
    let mine = |selector: &Value| pods.iter().copied().filter(|p| selects(selector, p)).collect::<Vec<_>>();
    // (a workload is its pods that haven't ended)
    let live_mine = |selector: &Value| mine(selector).into_iter().filter(|p| live(p)).collect::<Vec<_>>();
    match kind {
        "Deployment" | "StatefulSet" | "ReplicaSet" => {
            let want = spec["replicas"].as_u64().unwrap_or(1);
            let updated = (kind != "ReplicaSet").then(|| n(&st["updatedReplicas"]));
            replicas(n(&st["readyReplicas"]), want, updated, &live_mine(&spec["selector"]))
        }
        "DaemonSet" => replicas(
            n(&st["numberReady"]),
            n(&st["desiredNumberScheduled"]),
            Some(n(&st["updatedNumberScheduled"])),
            &live_mine(&spec["selector"]),
        ),
        "Pod" => match (pod_tone(o), pod_restarts(o)) {
            (said, 0) => said,
            ((s, tone), n) => (format!("{s} · {}", restarts(n)), tone),
        },
        "Job" => {
            let failed = condition(st, "Failed").filter(|c| c["status"] == "True");
            if let Some(c) = failed {
                (format!("failed · {}", text(&c["reason"])), Tone::Error)
            } else if condition(st, "Complete").is_some_and(|c| c["status"] == "True") {
                ("complete".into(), Tone::Ok)
            } else {
                let want = spec["completions"].as_u64().unwrap_or(1);
                (format!("{}/{want} done · {} running", n(&st["succeeded"]), n(&st["active"])), Tone::Info)
            }
        }
        "CronJob" => {
            if spec["suspend"] == true {
                ("suspended".into(), Tone::Warning)
            } else {
                let active = st["active"].as_array().map_or(0, Vec::len);
                let last = millis(&st["lastScheduleTime"])
                    .map(|t| format!("last run {} ago", human(tarae_plugin::now::millis() as i64 - t)))
                    .unwrap_or_else(|| "not run yet".into());
                let active = if active > 0 { format!(" · {active} running") } else { String::new() };
                (format!("{last}{active}"), Tone::Info)
            }
        }
        "Service" => {
            let kind = text(&spec["type"]);
            if kind == "ExternalName" {
                return (format!("→ {}", text(&spec["externalName"])), Tone::Ok);
            }
            let selector = &spec["selector"];
            if !selector.as_object().is_some_and(|m| !m.is_empty()) {
                return (format!("{kind} · no selector"), Tone::Info);
            }
            let ready = mine(selector).into_iter().filter(|p| serves(p)).count();
            match ready {
                0 => (format!("{kind} · no ready pods"), Tone::Warning),
                k => (format!("{kind} · {k} pod{} ready", if k == 1 { "" } else { "s" }), Tone::Ok),
            }
        }
        "Ingress" => {
            let at: Vec<String> = st["loadBalancer"]["ingress"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| i["ip"].as_str().or(i["hostname"].as_str()).map(str::to_string))
                .collect();
            if at.is_empty() { ("no address yet".into(), Tone::Warning) } else { (format!("→ {}", at.join(", ")), Tone::Ok) }
        }
        "PersistentVolumeClaim" => match st["phase"].as_str() {
            Some("Bound") => (format!("Bound · {}", text(&st["capacity"]["storage"])), Tone::Ok),
            Some(p) => (p.to_string(), Tone::Warning),
            None => ("pending".into(), Tone::Warning),
        },
        "HorizontalPodAutoscaler" => (
            format!("{} replicas (wants {})", n(&st["currentReplicas"]), n(&st["desiredReplicas"])),
            Tone::Info,
        ),
        "ConfigMap" => {
            let keys = o.data["data"].as_object().map_or(0, |m| m.len()) + o.data["binaryData"].as_object().map_or(0, |m| m.len());
            (format!("{keys} key{}", if keys == 1 { "" } else { "s" }), Tone::Ok)
        }
        // (its values aren't read — only that it's there)
        "Secret" => ("exists".into(), Tone::Ok),
        "Namespace" => match st["phase"].as_str() {
            Some("Active") => ("Active".into(), Tone::Ok),
            Some(p) => (p.to_string(), Tone::Warning),
            None => ("exists".into(), Tone::Ok),
        },
        // Anything else (a Flux HelmRelease, any CRD's objects): Ready if it says so — kstatus (above) says
        // the rest
        _ => match condition(st, "Ready") {
            Some(c) if c["status"] == "True" => ("Ready".into(), Tone::Ok),
            _ => ("exists".into(), Tone::Ok),
        },
    }
}

/// What the narrow side tab says of a state: a number or a word (`3/3`, `3 pods`, `Bound`) — the rest (its
/// pods' restarts too) is beside the code, and a problem's reason gets a line of its own.
pub fn brief(kind: &str, o: &DynamicObject, pods: &[&DynamicObject]) -> String {
    let (spec, st) = (&o.data["spec"], &o.data["status"]);
    let mine = |selector: &Value| pods.iter().copied().filter(|p| selects(selector, p)).collect::<Vec<_>>();
    match kind {
        "Deployment" | "StatefulSet" | "ReplicaSet" => {
            format!("{}/{}", n(&st["readyReplicas"]), spec["replicas"].as_u64().unwrap_or(1))
        }
        "DaemonSet" => format!("{}/{}", n(&st["numberReady"]), n(&st["desiredNumberScheduled"])),
        "Pod" => pod_status(o),
        "Job" => format!("{}/{}", n(&st["succeeded"]), spec["completions"].as_u64().unwrap_or(1)),
        "Service" if spec["selector"].as_object().is_some_and(|m| !m.is_empty()) => {
            let ready = mine(&spec["selector"]).into_iter().filter(|p| serves(p)).count();
            format!("{ready} pod{}", if ready == 1 { "" } else { "s" })
        }
        "Service" => text(&spec["type"]),
        "PersistentVolumeClaim" => st["phase"].as_str().unwrap_or("").to_string(),
        "HorizontalPodAutoscaler" => format!("{}/{}", n(&st["currentReplicas"]), n(&st["desiredReplicas"])),
        // (not fine: its reason has a line of its own)
        _ => match summary(kind, o, pods) {
            (said, Tone::Ok | Tone::Info) => said.split(" · ").next().unwrap_or("").to_string(),
            _ => String::new(),
        },
    }
}

/// How many times a pod's containers restarted.
pub fn pod_restarts(p: &DynamicObject) -> u64 {
    let st = &p.data["status"];
    ["containerStatuses", "initContainerStatuses"]
        .iter()
        .flat_map(|k| st[*k].as_array().into_iter().flatten())
        .filter_map(|c| c["restartCount"].as_u64())
        .sum()
}

/// Restarts in words (`3 restarts`) — a glyph for it (`↻`) isn't in every terminal font.
pub fn restarts(n: u64) -> String {
    format!("{n} restart{}", if n == 1 { "" } else { "s" })
}

/// A pod described, for a buffer: its state, where it runs, each container (image, state, restarts, how it
/// last ended), its conditions.
pub fn describe(p: &DynamicObject, now_ms: i64) -> String {
    let (spec, st) = (&p.data["spec"], &p.data["status"]);
    let ago = |v: &Value| millis(v).map(|t| format!("{} ago", human(now_ms - t))).unwrap_or_default();
    let mut out = format!(
        "Pod        {}  ·  {}\nStatus     {}   (phase {})\nNode       {}   IP {}\nStarted    {}\n\nContainers\n",
        p.metadata.name.clone().unwrap_or_default(),
        p.metadata.namespace.clone().unwrap_or_default(),
        pod_status(p),
        text(&st["phase"]),
        text(&spec["nodeName"]),
        text(&st["podIP"]),
        ago(&st["startTime"]),
    );
    let statuses = st["containerStatuses"].as_array().cloned().unwrap_or_default();
    for c in spec["containers"].as_array().into_iter().flatten() {
        let name = text(&c["name"]);
        let s = statuses.iter().find(|s| s["name"] == c["name"]).cloned().unwrap_or(Value::Null);
        out.push_str(&format!("  {name}   {}\n", text(&c["image"])));
        let state = &s["state"];
        let now = if let Some(w) = state["waiting"].as_object() {
            format!("waiting: {} — {}", text(&w["reason"].clone()), text(&w["message"].clone()))
        } else if state["running"].is_object() {
            format!("running since {}", ago(&state["running"]["startedAt"]))
        } else if state["terminated"].is_object() {
            format!("terminated: {}", ended(&state["terminated"]))
        } else {
            "not started".into()
        };
        out.push_str(&format!("    {}\n", now.trim_end_matches(" — ")));
        out.push_str(&format!(
            "    ready {} · restarts {}\n",
            if s["ready"] == true { "yes" } else { "no" },
            n(&s["restartCount"])
        ));
        let last = &s["lastState"]["terminated"];
        if last.is_object() {
            out.push_str(&format!("    last: {} {}\n", ended(last), ago(&last["finishedAt"])));
        }
    }
    out.push_str("\nConditions\n");
    for c in st["conditions"].as_array().into_iter().flatten() {
        let why = [text(&c["reason"]), text(&c["message"])].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>();
        out.push_str(&format!("  {:<16} {:<6} {}\n", text(&c["type"]), text(&c["status"]), why.join(": ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> DynamicObject {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn ages_as_kubectl_says_them() {
        let s = 1000;
        let cases = [
            (5 * s, "5s"),
            (119 * s, "119s"),
            (5 * 60 * s + 12 * s, "5m12s"),
            (7 * 60 * s, "7m"),
            (47 * 60 * s, "47m"),
            (3 * 3600 * s + 25 * 60 * s, "3h25m"),
            (20 * 3600 * s, "20h"),
            (3 * 86400 * s + 4 * 3600 * s, "3d4h"),
            (12 * 86400 * s, "12d"),
            (2 * 365 * 86400 * s + 30 * 86400 * s, "2y30d"),
            (9 * 365 * 86400 * s, "9y"),
        ];
        for (ms, said) in cases {
            assert_eq!(human(ms), said, "{ms}");
        }
    }

    #[test]
    fn pod_status_as_kubectl_works_it_out() {
        let base = |status: Value| {
            obj(json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "p"},
                "spec": {"containers": [{"name": "a"}], "initContainers": [{"name": "i"}]}, "status": status}))
        };
        let waiting = base(json!({"phase": "Running", "containerStatuses": [
            {"ready": false, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}));
        assert_eq!(pod_tone(&waiting), ("CrashLoopBackOff".into(), Tone::Error));
        let init = base(json!({"phase": "Pending", "initContainerStatuses": [
            {"state": {"waiting": {"reason": "PodInitializing"}}}]}));
        assert_eq!(pod_tone(&init), ("Init:0/1".into(), Tone::Warning));
        let init_failed = base(json!({"phase": "Pending", "initContainerStatuses": [
            {"state": {"terminated": {"exitCode": 2}}}]}));
        assert_eq!(pod_tone(&init_failed), ("Init:ExitCode:2".into(), Tone::Error));
        let evicted = base(json!({"phase": "Failed", "reason": "Evicted"}));
        assert_eq!(pod_status(&evicted), "Evicted");
        let done = base(json!({"phase": "Succeeded", "containerStatuses": [
            {"state": {"terminated": {"exitCode": 0, "reason": "Completed"}}}]}));
        assert_eq!(pod_tone(&done), ("Completed".into(), Tone::Ok));
        let mut going = base(json!({"phase": "Running"}));
        going.metadata.deletion_timestamp = serde_json::from_value(json!("2026-01-01T00:00:00Z")).ok();
        assert_eq!(pod_status(&going), "Terminating");
    }

    fn pod(name: &str, app: &str, state: Value) -> DynamicObject {
        obj(json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "labels": {"app": app}},
            "spec": {"containers": [{"name": "c"}]},
            "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": if state["running"].is_object() { "True" } else { "False" }}],
                "containerStatuses": [{"ready": state["running"].is_object(), "state": state}]}}))
    }

    #[test]
    fn workloads_count_ready_pods_and_say_what_is_wrong_with_the_others() {
        let deploy = |ready: u64, updated: u64| {
            obj(json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"},
                "spec": {"replicas": 3, "selector": {"matchLabels": {"app": "web"}}},
                "status": {"readyReplicas": ready, "updatedReplicas": updated}}))
        };
        let ok = pod("web-1", "web", json!({"running": {}}));
        let crash = pod("web-2", "web", json!({"waiting": {"reason": "CrashLoopBackOff"}}));
        let other = pod("api-1", "api", json!({"waiting": {"reason": "CrashLoopBackOff"}}));
        let pods = [&ok, &crash, &other];
        assert_eq!(summary("Deployment", &deploy(3, 3), &[&ok]), ("3/3 ready".into(), Tone::Ok));
        assert_eq!(
            summary("Deployment", &deploy(2, 3), &pods),
            ("2/3 ready · 1 CrashLoopBackOff".into(), Tone::Error),
            "only its own pods count"
        );
        assert_eq!(summary("Deployment", &deploy(2, 1), &[&ok]), ("2/3 ready · rolling out".into(), Tone::Warning));
        let svc = |sel: Value| obj(json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "web"}, "spec": {"type": "ClusterIP", "selector": sel}}));
        assert_eq!(summary("Service", &svc(json!({"app": "web"})), &pods), ("ClusterIP · 1 pod ready".into(), Tone::Ok));
        assert_eq!(summary("Service", &svc(json!({"app": "db"})), &pods), ("ClusterIP · no ready pods".into(), Tone::Warning));
    }

    /// A pod that ended (`Failed`: evicted, shut down with its node; `Succeeded`) stays in the API until it's
    /// collected — on a cluster whose nodes come and go (spot, Karpenter) there are always some. They say what
    /// happened then, not how the workload is now: a workload whose pods are all ready is fine.
    fn ended_pod(name: &str, app: &str, phase: &str, reason: &str, restarts: u64) -> DynamicObject {
        obj(json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "labels": {"app": app}},
            "spec": {"containers": [{"name": "c"}]},
            "status": {"phase": phase, "reason": reason, "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [{"ready": false, "restartCount": restarts,
                    "state": {"terminated": {"exitCode": if phase == "Failed" { 137 } else { 0 },
                        "reason": if phase == "Failed" { "Error" } else { "Completed" }}}}]}}))
    }

    #[test]
    fn pods_a_node_left_behind_dont_make_a_ready_workload_red() {
        let deploy = obj(json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "api"},
            "spec": {"replicas": 2, "selector": {"matchLabels": {"app": "api"}}},
            "status": {"readyReplicas": 2, "updatedReplicas": 2}}));
        let (a, b) = (pod("api-1", "api", json!({"running": {}})), pod("api-2", "api", json!({"running": {}})));
        let evicted = ended_pod("api-3", "api", "Failed", "Evicted", 11);
        let errored = ended_pod("api-4", "api", "Failed", "", 0);
        let done = ended_pod("api-5", "api", "Succeeded", "", 10);
        let pods = [&a, &b, &evicted, &errored, &done];
        assert_eq!(summary("Deployment", &deploy, &pods), ("2/2 ready".into(), Tone::Ok));
        // (one short of its replicas: a warning for that — not an error for pods that ended long ago)
        let short = obj(json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "api"},
            "spec": {"replicas": 3, "selector": {"matchLabels": {"app": "api"}}},
            "status": {"readyReplicas": 2, "updatedReplicas": 3}}));
        assert_eq!(summary("Deployment", &short, &pods), ("2/3 ready".into(), Tone::Warning));
    }

    #[test]
    fn restarts_are_the_live_pods_own() {
        let deploy = obj(json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"},
            "spec": {"replicas": 1, "selector": {"matchLabels": {"app": "web"}}},
            "status": {"readyReplicas": 1, "updatedReplicas": 1}}));
        let mut live = pod("web-1", "web", json!({"running": {}}));
        live.data["status"]["containerStatuses"][0]["restartCount"] = json!(2);
        // (an ended pod's restarts are its own history, gone with it)
        let gone = ended_pod("web-0", "web", "Succeeded", "", 10);
        assert_eq!(summary("Deployment", &deploy, &[&live, &gone]).0, "1/1 ready · 2 restarts");
    }

    /// A Service sends traffic only to pods that run and are ready — one that completed serves nothing.
    #[test]
    fn a_service_counts_only_pods_that_serve() {
        let svc = obj(json!({"apiVersion": "v1", "kind": "Service", "metadata": {"name": "web"},
            "spec": {"type": "ClusterIP", "selector": {"app": "web"}}}));
        let serving = pod("web-1", "web", json!({"running": {}}));
        let starting = pod("web-2", "web", json!({"waiting": {"reason": "ContainerCreating"}}));
        let (done, evicted) =
            (ended_pod("web-3", "web", "Succeeded", "", 0), ended_pod("web-4", "web", "Failed", "Evicted", 0));
        let pods = [&serving, &starting, &done, &evicted];
        assert_eq!(summary("Service", &svc, &pods), ("ClusterIP · 1 pod ready".into(), Tone::Ok));
        assert_eq!(brief("Service", &svc, &pods), "1 pod");
    }

    #[test]
    fn briefly_for_the_side_tab_and_described_for_a_buffer() {
        let deploy = obj(json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web"},
            "spec": {"replicas": 3, "selector": {"matchLabels": {"app": "web"}}}, "status": {"readyReplicas": 3, "updatedReplicas": 3}}));
        let mut ok = pod("web-1", "web", json!({"running": {"startedAt": "2026-01-01T00:00:00Z"}}));
        assert_eq!(brief("Deployment", &deploy, &[&ok]), "3/3");
        ok.data["status"]["containerStatuses"][0]["restartCount"] = json!(2);
        ok.data["status"]["containerStatuses"][0]["name"] = json!("c");
        ok.data["status"]["containerStatuses"][0]["lastState"] = json!({"terminated": {"exitCode": 137, "reason": "OOMKilled"}});
        assert_eq!(brief("Deployment", &deploy, &[&ok]), "3/3", "narrow: its pods' restarts are beside the code");
        assert_eq!(summary("Deployment", &deploy, &[&ok]).0, "3/3 ready · 2 restarts");
        assert_eq!(summary("Pod", &ok, &[]).0, "Running · 2 restarts");
        let said = describe(&ok, 0);
        assert!(said.contains("ready yes · restarts 2") && said.contains("last: OOMKilled"), "{said}");
    }

    #[test]
    fn anything_else_by_its_ready_condition() {
        let hr = |status: &str, reason: &str, msg: &str| {
            obj(json!({"apiVersion": "helm.toolkit.fluxcd.io/v2", "kind": "HelmRelease", "metadata": {"name": "web"},
                "status": {"conditions": [{"type": "Ready", "status": status, "reason": reason, "message": msg}]}}))
        };
        assert_eq!(summary("HelmRelease", &hr("True", "", ""), &[]), ("Ready".into(), Tone::Ok));
        // Not ready: still being worked on (kstatus: in progress) — failed once it's stalled
        assert_eq!(
            summary("HelmRelease", &hr("False", "InstallFailed", "chart not found"), &[]),
            ("InstallFailed: chart not found".into(), Tone::Warning)
        );
        let mut stalled = hr("False", "InstallFailed", "chart not found");
        stalled.data["status"]["conditions"].as_array_mut().unwrap().push(
            json!({"type": "Stalled", "status": "True", "reason": "RetriesExceeded", "message": "gave up"}),
        );
        assert_eq!(summary("HelmRelease", &stalled, &[]), ("RetriesExceeded: gave up".into(), Tone::Error));
        // A new generation its controller hasn't seen yet
        let mut behind = hr("True", "", "");
        behind.metadata.generation = Some(3);
        behind.data["status"]["observedGeneration"] = json!(2);
        assert_eq!(summary("HelmRelease", &behind, &[]), ("generation 3 not observed yet".into(), Tone::Warning));
    }
}
