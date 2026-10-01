//! Why an object's pods aren't fine, on the line of the manifest that's the cause — read from the pods'
//! own status (what the kubelet and the scheduler wrote there), nothing more asked of the cluster:
//!
//! - an image that can't be pulled: its `image:` line (`ImagePullBackOff: not found`);
//! - a ConfigMap or Secret a container needs that isn't there: the line that names it;
//! - killed for memory: its `limits.memory` line (`OOMKilled ↻3`); crashing: its `command:` (or `args:`, or
//!   its `name:`) line, with the exit code;
//! - running but not ready for over a minute: its `readinessProbe:` line;
//! - not scheduled: the requests, `nodeSelector` / `affinity`, `tolerations` or `volumes` the scheduler's
//!   message names.
//!
//! A container in the manifest is found by its name (by its place, when a template writes the name). A cause
//! whose line the manifest doesn't have is left to the `kind:` line's state.

use std::collections::BTreeMap;

use kube_core::DynamicObject;
use serde_json::Value;

use crate::fields::{Field, Seg};
use crate::tarae::plugin::types::Tone;

/// A cause, on its line.
#[derive(Clone, Debug, PartialEq)]
pub struct Cause {
    pub line: u64,
    pub text: String,
    pub tone: Tone,
}

/// How long a container may run not ready before it's said (a rollout's new pods take a moment).
const NOT_READY_MS: i64 = 60_000;
/// The longest a cause is said in.
const MAX_TEXT: usize = 72;

/// A container the manifest writes: which list it's in, where its item is, its place there, its name.
struct Written {
    list: String,
    at: Vec<Seg>,
    index: Option<usize>,
    name: Option<String>,
}

/// The containers a document writes (under any pod spec: a Deployment's, a CronJob's, a Pod's).
fn written(fields: &[Field]) -> Vec<Written> {
    let mut out: Vec<Written> = Vec::new();
    for f in fields {
        let Some(k) = f.path.windows(2).position(|w| {
            matches!(&w[0], Seg::Key(l) if l == "containers" || l == "initContainers")
                && matches!(w[1], Seg::Item(_) | Seg::Each)
        }) else {
            continue;
        };
        let at = f.path[..k + 2].to_vec();
        if out.iter().any(|w| w.at == at) {
            continue;
        }
        let Seg::Key(list) = &at[k] else { continue };
        let index = match at[k + 1] {
            Seg::Item(i) => Some(i),
            _ => None,
        };
        let mut name_path = at.clone();
        name_path.push(Seg::Key("name".into()));
        let name = fields
            .iter()
            .find(|f| f.path == name_path && !f.templated && !f.nested)
            .and_then(|f| f.value.as_str().map(str::to_string));
        out.push(Written { list: list.clone(), at, index, name });
    }
    out
}

/// The line of the field at `path`, if the manifest writes it — itself, or in a flow mapping that has it
/// (`requests: {cpu: "8"}`).
fn line_at(fields: &[Field], path: &[Seg]) -> Option<u64> {
    fields.iter().find(|f| f.path == path).map(|f| f.line).or_else(|| {
        fields
            .iter()
            .find(|f| {
                f.value.is_object()
                    && path.starts_with(&f.path)
                    && crate::fields::at(&f.value, &path[f.path.len()..]).is_some()
            })
            .map(|f| f.line)
    })
}

fn with(at: &[Seg], keys: &[&str]) -> Vec<Seg> {
    let mut p = at.to_vec();
    p.extend(keys.iter().map(|k| Seg::Key(k.to_string())));
    p
}

/// The manifest's container a pod's container status is of: by name, else (a template's name) by place.
fn container<'a>(ws: &'a [Written], list: &str, name: &str, pod: &Value) -> Option<&'a Written> {
    let mine: Vec<&Written> = ws.iter().filter(|w| w.list == list).collect();
    if let Some(w) = mine.iter().find(|w| w.name.as_deref() == Some(name)) {
        return Some(w);
    }
    let place = pod["spec"][list].as_array()?.iter().position(|c| c["name"] == name)?;
    mine.into_iter().find(|w| w.name.is_none() && w.index == Some(place))
}

/// The end of a long message (`…: not found`), or all of it.
fn short(message: &str) -> &str {
    message.rsplit(": ").next().unwrap_or(message).trim()
}

fn cut(s: String) -> String {
    if s.chars().count() > MAX_TEXT { format!("{}…", s.chars().take(MAX_TEXT - 1).collect::<String>()) } else { s }
}

/// The name a message puts in quotes (`configmap "web-settings" not found`).
fn quoted(message: &str) -> Option<&str> {
    let (_, rest) = message.split_once('"')?;
    Some(rest.split_once('"')?.0)
}

/// The causes of these pods' troubles in a document whose fields are `fields` — each once, with how many
/// pods have it; `now` for how long a container has run.
pub fn causes(fields: &[Field], pods: &[&DynamicObject], now: i64) -> Vec<Cause> {
    let ws = written(fields);
    // The pod spec the containers are in (`spec.template.spec`): what the scheduler's reasons are about
    let spec = ws.first().map(|w| w.at[..w.at.len() - 2].to_vec());
    let mut found: BTreeMap<(u64, String), (Tone, usize)> = BTreeMap::new();
    for p in pods.iter().filter(|p| p.metadata.deletion_timestamp.is_none()) {
        let pod = &p.data;
        let mut mine: Vec<(u64, String, Tone)> = Vec::new();
        for (list, key) in [("containers", "containerStatuses"), ("initContainers", "initContainerStatuses")] {
            for st in pod["status"][key].as_array().into_iter().flatten() {
                let name = st["name"].as_str().unwrap_or_default();
                let Some(w) = container(&ws, list, name, pod) else { continue };
                let restarts = st["restartCount"].as_u64().unwrap_or(0);
                let waiting = &st["state"]["waiting"];
                let reason = waiting["reason"].as_str().unwrap_or_default();
                let message = waiting["message"].as_str().unwrap_or_default();
                let ended = if st["state"]["terminated"].is_object() {
                    &st["state"]["terminated"]
                } else {
                    &st["lastState"]["terminated"]
                };
                let line = |keys: &[&str]| line_at(fields, &with(&w.at, keys));
                match reason {
                    "ImagePullBackOff" | "ErrImagePull" | "InvalidImageName" | "ErrImageNeverPull" => {
                        if let Some(l) = line(&["image"]) {
                            mine.push((l, format!("{reason}: {}", short(message)), Tone::Error));
                        }
                    }
                    "CreateContainerConfigError" | "CreateContainerError" => {
                        // The line naming what's missing, in the container or the pod's volumes — or its name
                        let missing = quoted(message);
                        let names = |v: &Value, m: &str| {
                            v.as_str() == Some(m) || v.as_object().is_some_and(|o| o.values().any(|x| x == m))
                        };
                        let named = missing.and_then(|m| {
                            fields.iter().find(|f| {
                                names(&f.value, m)
                                    && (f.path.starts_with(&w.at)
                                        || spec.as_ref().is_some_and(|s| f.path.starts_with(&with(s, &["volumes"]))))
                            })
                        });
                        if let Some(l) = named.map(|f| f.line).or_else(|| line(&["name"])) {
                            mine.push((l, format!("{reason}: {message}"), Tone::Error));
                        }
                    }
                    _ if ended["reason"] == "OOMKilled" => {
                        if let Some(l) = line(&["resources", "limits", "memory"]).or_else(|| line(&["name"])) {
                            mine.push((l, format!("OOMKilled ↻{restarts}"), Tone::Error));
                        }
                    }
                    "CrashLoopBackOff" => {
                        let code = ended["exitCode"].as_i64().map_or(String::new(), |c| format!(" · exit {c}"));
                        let l = line(&["command"]).or_else(|| line(&["args"])).or_else(|| line(&["name"]));
                        if let Some(l) = l {
                            mine.push((l, format!("CrashLoopBackOff{code} ↻{restarts}"), Tone::Error));
                        }
                    }
                    _ => {}
                }
                // Running, not ready, for a while
                let since = crate::status::millis(&st["state"]["running"]["startedAt"]);
                if st["ready"] == false
                    && since.is_some_and(|t| now - t > NOT_READY_MS)
                    && let Some(l) = line(&["readinessProbe"])
                {
                    mine.push((l, "not ready (readinessProbe)".into(), Tone::Warning));
                }
            }
        }
        // Not scheduled: what the scheduler says it couldn't satisfy
        let unscheduled = pod["status"]["conditions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["type"] == "PodScheduled" && c["status"] == "False" && c["reason"] == "Unschedulable");
        if let (Some(c), Some(spec)) = (unscheduled, &spec) {
            let message = c["message"].as_str().unwrap_or_default();
            let why = message.split_once("available: ").map_or(message, |(_, r)| r);
            let why = why.split(". ").next().unwrap_or(why).trim_end_matches('.');
            // Each of its reasons on the line it's about (`1 Insufficient cpu` on the cpu request)
            let clauses: Vec<&str> = why.split(", ").collect();
            let clause = |said: &str| clauses.iter().find(|c| c.to_lowercase().contains(said)).copied();
            let on = |line: Option<u64>, said: &str| line.zip(clause(said));
            let request = |kind: &str| {
                ws.iter().filter(|w| w.list == "containers").find_map(|w| line_at(fields, &with(&w.at, &["resources", "requests", kind])))
            };
            let placed = line_at(fields, &with(spec, &["nodeSelector"])).or_else(|| line_at(fields, &with(spec, &["affinity"])));
            let reasons = [
                on(request("cpu"), "insufficient cpu"),
                on(request("memory"), "insufficient memory"),
                on(placed, "node affinity").or_else(|| on(placed, "node selector")),
                on(line_at(fields, &with(spec, &["tolerations"])), "taint"),
                on(line_at(fields, &with(spec, &["volumes"])), "persistentvolumeclaim"),
            ];
            mine.extend(reasons.into_iter().flatten().map(|(l, c)| (l, format!("Unschedulable: {c}"), Tone::Warning)));
        }
        // (each cause once per pod)
        mine.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        mine.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
        for (line, text, tone) in mine {
            found.entry((line, text)).or_insert((tone, 0)).1 += 1;
        }
    }
    found
        .into_iter()
        .map(|((line, text), (tone, n))| {
            let text = if n > 1 { format!("{text} ({n} pods)") } else { text };
            Cause { line, text: cut(text), tone }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MANIFEST: &str = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    spec:
      nodeSelector:
        disk: ssd
      containers:
        - name: web
          image: nginx:no-such-tag
          command: [nginx]
          readinessProbe:
            httpGet: {path: /, port: 80}
          resources:
            requests: {cpu: \"8\"}
            limits:
              memory: 64Mi
          env:
            - name: A
              valueFrom:
                configMapKeyRef: {name: web-settings, key: a}
        - name: side
          image: busybox
";

    fn pod(name: &str, status: Value) -> DynamicObject {
        serde_json::from_value(json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name},
            "spec": {"containers": [{"name": "web"}, {"name": "side"}]}, "status": status}))
        .unwrap()
    }

    fn said(pods: &[DynamicObject], now: i64) -> Vec<(u64, String, Tone)> {
        let lines: Vec<&str> = MANIFEST.lines().collect();
        let fields = crate::fields::fields(&lines, 0);
        let pods: Vec<&DynamicObject> = pods.iter().collect();
        causes(&fields, &pods, now).into_iter().map(|c| (c.line, c.text, c.tone)).collect()
    }

    fn waiting(container: &str, reason: &str, message: &str) -> Value {
        json!({"name": container, "ready": false, "restartCount": 0, "state": {"waiting": {"reason": reason, "message": message}}})
    }

    #[test]
    fn an_image_that_can_t_be_pulled_on_its_line_once_with_how_many_pods() {
        let bad = || json!({"containerStatuses": [waiting("web", "ImagePullBackOff",
            "Back-off pulling image \"nginx:no-such-tag\": ErrImagePull: failed to resolve reference: docker.io/library/nginx:no-such-tag: not found")]});
        let got = said(&[pod("a", bad()), pod("b", bad())], 0);
        assert_eq!(got, [(11, "ImagePullBackOff: not found (2 pods)".to_string(), Tone::Error)]);
    }

    #[test]
    fn a_missing_config_map_on_the_line_that_names_it() {
        let got = said(&[pod("a", json!({"containerStatuses": [waiting("web", "CreateContainerConfigError", "configmap \"web-settings\" not found")]}))], 0);
        assert_eq!(got, [(22, "CreateContainerConfigError: configmap \"web-settings\" not found".to_string(), Tone::Error)]);
    }

    #[test]
    fn killed_for_memory_on_its_limit_crashing_on_its_command_not_ready_on_its_probe() {
        let oom = json!({"containerStatuses": [{"name": "web", "ready": false, "restartCount": 3,
            "state": {"waiting": {"reason": "CrashLoopBackOff"}}, "lastState": {"terminated": {"reason": "OOMKilled", "exitCode": 137}}}]});
        assert_eq!(said(&[pod("a", oom)], 0), [(18, "OOMKilled ↻3".to_string(), Tone::Error)]);
        let crash = json!({"containerStatuses": [{"name": "web", "ready": false, "restartCount": 2,
            "state": {"waiting": {"reason": "CrashLoopBackOff"}}, "lastState": {"terminated": {"reason": "Error", "exitCode": 1}}}]});
        assert_eq!(said(&[pod("a", crash)], 0), [(12, "CrashLoopBackOff · exit 1 ↻2".to_string(), Tone::Error)]);
        let unready = |since: &str| json!({"containerStatuses": [{"name": "web", "ready": false, "restartCount": 0,
            "state": {"running": {"startedAt": since}}}]});
        let at = crate::status::millis(&json!("2026-10-01T00:00:00Z")).unwrap();
        assert!(said(&[pod("a", unready("2026-10-01T00:00:00Z"))], at + 30_000).is_empty(), "a moment: not yet");
        assert_eq!(
            said(&[pod("a", unready("2026-10-01T00:00:00Z"))], at + 90_000),
            [(13, "not ready (readinessProbe)".to_string(), Tone::Warning)]
        );
    }

    #[test]
    fn not_scheduled_on_what_the_scheduler_couldn_t_satisfy() {
        let pending = json!({"conditions": [{"type": "PodScheduled", "status": "False", "reason": "Unschedulable",
            "message": "0/3 nodes are available: 1 Insufficient cpu, 2 node(s) didn't match Pod's node affinity/selector. preemption: 0/3 nodes are available."}]});
        let got = said(&[pod("a", pending)], 0);
        assert_eq!(
            got,
            [
                (7, "Unschedulable: 2 node(s) didn't match Pod's node affinity/selector".to_string(), Tone::Warning),
                (16, "Unschedulable: 1 Insufficient cpu".to_string(), Tone::Warning)
            ]
        );
    }

    #[test]
    fn a_template_s_container_found_by_its_place() {
        let text = "\
spec:
  template:
    spec:
      containers:
        - name: {{ .Chart.Name }}
          image: \"{{ .Values.image }}\"
";
        let lines: Vec<&str> = text.lines().collect();
        let fields = crate::fields::fields(&lines, 0);
        let p: DynamicObject = serde_json::from_value(json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "a"},
            "spec": {"containers": [{"name": "shop"}]},
            "status": {"containerStatuses": [waiting("shop", "ErrImagePull", "rpc error: pull access denied")]}})).unwrap();
        let got: Vec<(u64, String)> = causes(&fields, &[&p], 0).into_iter().map(|c| (c.line, c.text)).collect();
        assert_eq!(got, [(5, "ErrImagePull: pull access denied".to_string())]);
    }
}
