//! Which field each line of a manifest writes: a line-by-line reading of block YAML (`key: value`,
//! `- item`, nested by indentation, a sequence indented or not) — enough to put the cluster's value beside
//! the line that says it. A block scalar (`|`, `>`) and anything that isn't read cleanly is left out: a
//! value is said only where the line and the field are sure to be the same.
//!
//! Helm templates read the same way: a value with `{{ }}` in it is the cluster's to say (`= 2`), a line
//! that's only a template action (`{{- if }}`, `{{- end }}`, `{{- toYaml … | nindent 8 }}`) is passed over,
//! and an item inside `{{- range }}` stands for every item (its values, a list).

use serde_json::Value;

/// A step into an object: a key, or a sequence's item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Item(usize),
    /// Every item (a template's `range`).
    Each,
}

/// A line that writes a value: (its line in the document, from 0; where; the value as written).
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub line: u64,
    pub path: Vec<Seg>,
    pub value: Value,
    /// Written by a template (`{{ }}`): the cluster's value is said, not compared — `value` is the template's
    /// text (`{{ .Values.replicaCount }}`).
    pub templated: bool,
    /// A key whose value is below it (`resources:`): where it is, not a value.
    pub nested: bool,
}

/// What an indentation level is inside of: a key (its value below), or a sequence's item.
#[derive(Debug)]
struct Frame {
    indent: usize,
    seg: Seg,
    /// For an item: its sequence's indentation (the next `-` there is the next item).
    dash: Option<usize>,
}

/// A scalar or a flow collection, as written — none if it isn't one to compare (a block scalar, an
/// anchor, an alias, a tag).
fn scalar(text: &str) -> Option<Value> {
    let t = text.trim();
    // (a comment after it)
    let t = match t.find(" #") {
        Some(i) if !t.starts_with(['"', '\'']) => t[..i].trim_end(),
        _ => t,
    };
    if t.is_empty() || t.starts_with(['|', '>', '&', '*', '!']) {
        return None;
    }
    let v: serde_yaml::Value = serde_yaml::from_str(t).ok()?;
    serde_json::to_value(v).ok()
}

/// The fields a document's lines write, from its first line (`first`).
pub fn fields(lines: &[&str], first: u64) -> Vec<Field> {
    let mut out = Vec::new();
    let mut stack: Vec<Frame> = Vec::new();
    // Each sequence's next item, by its path
    let mut next: Vec<(Vec<Seg>, usize)> = Vec::new();
    let mut skip_below: Option<usize> = None;
    // The template blocks open (`range` or not), for `{{- range }}` … `{{- end }}`
    let mut blocks: Vec<bool> = Vec::new();
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.trim_end();
        let body = line.trim_start();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        // A line that's only template actions: passed over (not an indentation level)
        if body.starts_with("{{") && body.ends_with("}}") {
            for action in body.split("{{").skip(1) {
                let word =
                    action.trim_start_matches('-').trim_start().split_whitespace().next().unwrap_or("");
                match word {
                    "range" => blocks.push(true),
                    "if" | "with" | "define" | "block" => blocks.push(false),
                    "end" => {
                        blocks.pop();
                    }
                    _ => {}
                }
            }
            continue;
        }
        let indent = line.len() - body.len();
        // Under a block scalar: its text, not fields
        match skip_below {
            Some(d) if indent > d => continue,
            _ => skip_below = None,
        }
        if body == "---" || body == "..." {
            stack.clear();
            continue;
        }
        let (dash, rest, at) = match body.strip_prefix("- ").or_else(|| (body == "-").then_some("")) {
            Some(rest) => (true, rest, indent + 2 + (rest.len() - rest.trim_start().len())),
            None => (false, body, indent),
        };
        // Out of what this line isn't inside of — a `-` at a key's own indentation is that key's item
        while let Some(f) = stack.last() {
            let keep = if dash {
                f.indent < indent || (f.indent == indent && matches!(f.seg, Seg::Key(_)) && f.dash.is_none())
            } else {
                f.indent < indent
            };
            if keep {
                break;
            }
            stack.pop();
        }
        if dash {
            let path: Vec<Seg> = stack.iter().map(|f| f.seg.clone()).collect();
            let n = match next.iter_mut().find(|(p, _)| *p == path) {
                Some((_, n)) => {
                    *n += 1;
                    *n - 1
                }
                None => {
                    next.push((path.clone(), 1));
                    0
                }
            };
            // (a sequence inside it counts on its own: its path has this item in it)
            // (inside a range: every item)
            let seg = if blocks.contains(&true) { Seg::Each } else { Seg::Item(n) };
            stack.push(Frame { indent, seg, dash: Some(indent) });
            if rest.is_empty() {
                continue;
            }
        }
        let rest = rest.trim_start();
        let path_here = |stack: &Vec<Frame>| stack.iter().map(|f| f.seg.clone()).collect::<Vec<Seg>>();
        // `key: value`, or `key:` with its value below
        match split_key(rest) {
            Some((key, value)) => {
                let value = value.trim();
                if value.is_empty() {
                    let mut path = path_here(&stack);
                    path.push(Seg::Key(key.clone()));
                    out.push(Field { line: first + i as u64, path, value: Value::Null, templated: false, nested: true });
                    stack.push(Frame { indent: at, seg: Seg::Key(key), dash: None });
                } else if value.starts_with(['|', '>']) {
                    skip_below = Some(at);
                } else if value.contains("{{") {
                    let mut path = path_here(&stack);
                    path.push(Seg::Key(key));
                    let text = Value::String(value.to_string());
                    out.push(Field { line: first + i as u64, path, value: text, templated: true, nested: false });
                } else if let Some(v) = scalar(value) {
                    let mut path = path_here(&stack);
                    path.push(Seg::Key(key));
                    out.push(Field { line: first + i as u64, path, value: v, templated: false, nested: false });
                }
            }
            // `- value`: the item itself
            None if dash => {
                let (value, templated) = match rest.contains("{{") {
                    true => (Some(Value::String(rest.trim().to_string())), true),
                    false => (scalar(rest), false),
                };
                if let Some(value) = value {
                    out.push(Field { line: first + i as u64, path: path_here(&stack), value, templated, nested: false });
                }
            }
            None => {}
        }
    }
    out
}

/// `key: value` → (key, value); none if the line isn't a mapping entry (a quoted key allowed).
fn split_key(s: &str) -> Option<(String, &str)> {
    if let Some(q) = s.chars().next().filter(|c| *c == '"' || *c == '\'') {
        let end = s[1..].find(q)? + 1;
        let rest = s[end + 1..].strip_prefix(':')?;
        return (rest.is_empty() || rest.starts_with(' ')).then(|| (s[1..end].to_string(), rest));
    }
    let i = s.find(": ").or_else(|| s.ends_with(':').then(|| s.len() - 1))?;
    let key = &s[..i];
    // (a flow collection or a scalar with `: ` inside isn't a key)
    if key.is_empty() || key.contains(['{', '[', '#']) {
        return None;
    }
    Some((key.to_string(), &s[i + 1..]))
}

/// The value at `path` in `v` — for every item (`Each`), each's, as a list.
pub fn at(v: &Value, path: &[Seg]) -> Option<Value> {
    match path.split_first() {
        None => Some(v.clone()),
        Some((Seg::Key(k), rest)) => at(v.get(k)?, rest),
        Some((Seg::Item(n), rest)) => at(v.get(*n)?, rest),
        Some((Seg::Each, rest)) => {
            let all: Option<Vec<Value>> = v.as_array()?.iter().map(|x| at(x, rest)).collect();
            Some(Value::Array(all?))
        }
    }
}

/// A quantity (`500m`, `0.5`, `1Gi`, `1024Mi`, `2e3`) as a number, for comparing how it's written.
fn quantity(v: &Value) -> Option<f64> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    let s = s.trim();
    let units: [(&str, f64); 12] = [
        ("Ki", 1024.0),
        ("Mi", 1024f64.powi(2)),
        ("Gi", 1024f64.powi(3)),
        ("Ti", 1024f64.powi(4)),
        ("Pi", 1024f64.powi(5)),
        ("Ei", 1024f64.powi(6)),
        ("m", 1e-3),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
    ];
    for (u, f) in units {
        if let Some(n) = s.strip_suffix(u) {
            return n.parse::<f64>().ok().map(|n| n * f);
        }
    }
    s.parse::<f64>().ok()
}

/// Whether the cluster's value is what the line says — quantities by amount under `resources`, numbers
/// written as text as numbers.
pub fn same(path: &[Seg], written: &Value, live: &Value) -> bool {
    if written == live {
        return true;
    }
    // A flow collection: what's written, each by what it means (the cluster may have more keys)
    match (written, live) {
        (Value::Object(w), Value::Object(l)) => {
            return w.iter().all(|(k, v)| {
                let mut p = path.to_vec();
                p.push(Seg::Key(k.clone()));
                l.get(k).is_some_and(|x| same(&p, v, x))
            });
        }
        (Value::Array(w), Value::Array(l)) => {
            return w.len() == l.len()
                && w.iter().zip(l).enumerate().all(|(i, (a, b))| {
                    let mut p = path.to_vec();
                    p.push(Seg::Item(i));
                    same(&p, a, b)
                });
        }
        _ => {}
    }
    let resources = path.iter().any(|s| matches!(s, Seg::Key(k) if k == "resources"));
    if resources && let (Some(a), Some(b)) = (quantity(written), quantity(live)) {
        return (a - b).abs() <= a.abs().max(b.abs()) * 1e-9;
    }
    match (written, live) {
        (Value::String(a), Value::Number(b)) | (Value::Number(b), Value::String(a)) => a == &b.to_string(),
        _ => false,
    }
}

/// Who holds the field at `path` in the cluster's object (`live`): the managers whose `managedFields` own
/// it (server-side apply's record — `kubectl-edit`, a controller, `helm`, `kustomize-controller`, …). A
/// list's item is found by its key there (`k:{"name":"web"}`), or by its value (`v:…`). The one that
/// wrote last first.
pub fn managers(live: &Value, path: &[Seg]) -> Vec<String> {
    let Some(all) = live["metadata"]["managedFields"].as_array() else { return Vec::new() };
    let mut all: Vec<&Value> = all.iter().collect();
    // (RFC 3339 times sort as text)
    all.sort_by(|a, b| b["time"].as_str().unwrap_or("").cmp(a["time"].as_str().unwrap_or("")));
    let mut out: Vec<String> = Vec::new();
    for m in all {
        let (Some(name), Some(mut node)) = (m["manager"].as_str(), m.get("fieldsV1")) else { continue };
        let mut here = live;
        let mut owns = true;
        for seg in path {
            let next = match seg {
                Seg::Key(k) => {
                    here = &here[k.as_str()];
                    node.get(format!("f:{k}"))
                }
                // (every item: no one manager's)
                Seg::Each => None,
                Seg::Item(n) => {
                    let item = &here[*n];
                    here = item;
                    node.as_object().and_then(|o| {
                        o.iter().find_map(|(key, v)| {
                            let hit = match (key.strip_prefix("k:"), key.strip_prefix("v:")) {
                                (Some(k), _) => serde_json::from_str::<Value>(k).ok().is_some_and(|k| {
                                    k.as_object().is_some_and(|k| k.iter().all(|(f, x)| item.get(f) == Some(x)))
                                }),
                                (_, Some(v)) => serde_json::from_str::<Value>(v).ok().as_ref() == Some(item),
                                _ => false,
                            };
                            hit.then_some(v)
                        })
                    })
                }
            };
            match next {
                Some(n) => node = n,
                None => {
                    owns = false;
                    break;
                }
            }
        }
        if owns && !out.iter().any(|o| o == name) {
            out.push(name.to_string());
        }
    }
    out
}

/// A value said beside a line: a scalar as is, anything else short.
pub fn told(v: &Value) -> String {
    let s = match v {
        Value::String(s) => s.clone(),
        // (a range's: its items' values, the first few)
        Value::Array(items) if items.iter().all(|x| !x.is_object() && !x.is_array()) && !items.is_empty() => {
            let each: Vec<String> = items.iter().take(3).map(told).collect();
            let more = if items.len() > 3 { format!(", … ({})", items.len()) } else { String::new() };
            format!("{}{more}", each.join(", "))
        }
        v => v.to_string(),
    };
    if s.chars().count() > 48 { format!("{}…", s.chars().take(47).collect::<String>()) } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read_path(path: &[Seg]) -> String {
        path.iter()
            .map(|s| match s {
                Seg::Key(k) => k.clone(),
                Seg::Item(n) => format!("[{n}]"),
                Seg::Each => "[*]".into(),
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    /// The values the lines write (not the keys with theirs below).
    fn read(text: &str) -> Vec<(u64, String, Value)> {
        let lines: Vec<&str> = text.lines().collect();
        fields(&lines, 0)
            .into_iter()
            .filter(|f| !f.nested)
            .map(|f| (f.line, read_path(&f.path), if f.templated { Value::String("{{}}".into()) } else { f.value }))
            .collect()
    }

    #[test]
    fn each_line_s_field_indented_sequences_or_not() {
        let text = "\
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  labels: {app: web, tier: \"front\"}
spec:
  replicas: 3   # three
  template:
    spec:
      containers:
        - name: web
          image: nginx:1.27
          ports:
            - containerPort: 80
          env:
          - name: A
            value: \"1\"
          - name: B
            value: two
        - name: side
          args: [--x, --y]
          command:
            - sh
            - -c
          script: |
            echo hi
            a: b
      restartPolicy: Always
";
        let got = read(text);
        let find = |p: &str| got.iter().find(|(_, q, _)| q == p).map(|(l, _, v)| (*l, v.clone()));
        assert_eq!(find("spec.replicas"), Some((6, json!(3))));
        assert_eq!(find("metadata.labels"), Some((4, json!({"app": "web", "tier": "front"}))));
        assert_eq!(find("spec.template.spec.containers.[0].image"), Some((11, json!("nginx:1.27"))));
        assert_eq!(find("spec.template.spec.containers.[0].ports.[0].containerPort"), Some((13, json!(80))));
        assert_eq!(find("spec.template.spec.containers.[0].env.[1].value"), Some((18, json!("two"))), "an indentless sequence");
        assert_eq!(find("spec.template.spec.containers.[1].name"), Some((19, json!("side"))));
        assert_eq!(find("spec.template.spec.containers.[1].args"), Some((20, json!(["--x", "--y"]))));
        assert_eq!(find("spec.template.spec.containers.[1].command.[1]"), Some((23, json!("-c"))));
        assert_eq!(find("spec.template.spec.restartPolicy"), Some((27, json!("Always"))), "after a block scalar");
        assert!(find("spec.template.spec.containers.[1].a").is_none(), "a block scalar's text isn't a field");
        // A key whose value is below it: where it is
        let nested = |p: &str| {
            fields(&text.lines().collect::<Vec<_>>(), 0).into_iter().find(|f| f.nested && read_path(&f.path) == p).map(|f| f.line)
        };
        assert_eq!(nested("spec.template.spec.containers.[1].command"), Some(21));
        assert_eq!(nested("spec.template.spec.containers"), Some(9));
        assert_eq!(nested("spec.template.spec.containers.[1].script"), None, "a block scalar's key: a value, not nested");
    }

    #[test]
    fn a_template_s_lines_its_values_the_cluster_s_actions_passed_over_ranges_every_item() {
        let text = "\
spec:
  {{- if not .Values.autoscaling.enabled }}
  replicas: {{ .Values.replicaCount }}
  {{- end }}
  template:
    spec:
      containers:
        - name: {{ .Chart.Name }}
          {{- with .Values.securityContext }}
          securityContext:
            {{- toYaml . | nindent 12 }}
          {{- end }}
          image: \"{{ .Values.image.repository }}:{{ .Values.image.tag }}\"
          env:
          {{- range .Values.env }}
            - name: {{ .name }}
              value: {{ .value | quote }}
          {{- end }}
          imagePullPolicy: IfNotPresent
";
        let got: Vec<(u64, String, bool)> =
            read(text).into_iter().map(|(l, p, v)| (l, p, v == "{{}}")).collect();
        let find = |p: &str| got.iter().find(|(_, q, _)| q == p).map(|(l, _, t)| (*l, *t));
        assert_eq!(find("spec.replicas"), Some((2, true)), "inside an if");
        assert_eq!(find("spec.template.spec.containers.[0].name"), Some((7, true)));
        assert_eq!(find("spec.template.spec.containers.[0].image"), Some((12, true)), "after a with…end");
        assert_eq!(find("spec.template.spec.containers.[0].env.[*].value"), Some((16, true)), "every item");
        assert_eq!(find("spec.template.spec.containers.[0].imagePullPolicy"), Some((18, false)));
    }

    #[test]
    fn the_cluster_s_value_compared_as_it_means() {
        let res = [Seg::Key("resources".into()), Seg::Key("limits".into()), Seg::Key("cpu".into())];
        assert!(same(&res, &json!("0.5"), &json!("500m")));
        assert!(same(&res, &json!("1Gi"), &json!("1024Mi")));
        assert!(!same(&res, &json!("1Gi"), &json!("512Mi")));
        assert!(same(&[Seg::Key("port".into())], &json!("80"), &json!(80)));
        assert!(!same(&[Seg::Key("replicas".into())], &json!(1), &json!(2)));
        let live = json!({"spec": {"containers": [{"image": "a"}, {"image": "b"}]}});
        // Who holds a field: by the managers' records, a list's item by its key
        let owned = json!({"metadata": {"managedFields": [
            {"manager": "kubectl-client-side-apply", "time": "2026-01-01T00:00:00Z", "fieldsV1": {"f:spec": {"f:replicas": {}, "f:template": {"f:spec": {"f:containers": {"k:{\"name\":\"web\"}": {"f:image": {}}}}}}}},
            {"manager": "kube-controller-manager", "time": "2026-02-01T00:00:00Z", "operation": "Update", "subresource": "scale", "fieldsV1": {"f:spec": {"f:replicas": {}}}}
        ]}, "spec": {"replicas": 3, "template": {"spec": {"containers": [{"name": "web", "image": "x"}]}}}});
        let replicas = [Seg::Key("spec".into()), Seg::Key("replicas".into())];
        assert_eq!(managers(&owned, &replicas), ["kube-controller-manager", "kubectl-client-side-apply"], "last first");
        let image = [Seg::Key("spec".into()), Seg::Key("template".into()), Seg::Key("spec".into()), Seg::Key("containers".into()), Seg::Item(0), Seg::Key("image".into())];
        assert_eq!(managers(&owned, &image), ["kubectl-client-side-apply"]);
        let p =
            [Seg::Key("spec".into()), Seg::Key("containers".into()), Seg::Item(1), Seg::Key("image".into())];
        assert_eq!(at(&live, &p), Some(json!("b")));
        let each =
            [Seg::Key("spec".into()), Seg::Key("containers".into()), Seg::Each, Seg::Key("image".into())];
        assert_eq!(at(&live, &each), Some(json!(["a", "b"])));
        assert_eq!(told(&json!(["a", "b", "c", "d"])), "a, b, c, … (4)");
    }
}
