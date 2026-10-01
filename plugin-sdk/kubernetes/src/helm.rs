//! Helm, as far as the plugin needs it — and only as Helm itself does it, so it's the same whoever installed
//! the chart (the helm CLI, or a GitOps controller that renders it): the project's charts (each `Chart.yaml`
//! and its name), a template file's documents (the `kind:` each writes out; its values, template or not),
//! the releases of a chart the cluster has (its objects' `helm.sh/chart: <chart>-<version>` label, and
//! `app.kubernetes.io/instance: <release>`), and `helm template` for a release: each object it puts out
//! says which template it came from (`# Source: <chart>/templates/<file>`), so the k-th object of a kind
//! from a file is that file's k-th document of the kind.

use std::collections::BTreeMap;

use crate::kustomize::{parent, relative};
use crate::manifest::{Doc, Via};

/// The most documents read from one template file.
const MAX_DOCS: usize = 200;

/// Whether a file is a chart's template (by where it is: `…/templates/…`, YAML).
pub fn is_template(path: &str) -> bool {
    path.contains("/templates/") && (path.ends_with(".yaml") || path.ends_with(".yml"))
}

/// A `Chart.yaml`'s chart name.
pub fn chart_name(text: &str) -> Option<String> {
    let v: serde_yaml::Value = serde_yaml::from_str(text).ok()?;
    v["name"].as_str().filter(|n| !n.is_empty()).map(str::to_string)
}

/// The chart that renders a template file — the outermost (a subchart's templates, under its parent's
/// `charts/`, are rendered by its parent) — as (its folder, what `# Source:` calls the file).
pub fn source_of(charts: &BTreeMap<String, String>, file: &str) -> Option<(String, String)> {
    // The chart it's in: the nearest folder with a Chart.yaml whose templates hold it
    let mut dir = charts
        .keys()
        .filter(|d| file.starts_with(&format!("{d}/templates/")))
        .max_by_key(|d| d.len())?
        .clone();
    // Its parent's, while it's one of `charts/`
    loop {
        let up = parent(&dir);
        if !up.ends_with("/charts") || !charts.contains_key(parent(up)) {
            break;
        }
        dir = parent(up).to_string();
    }
    let name = &charts[&dir];
    Some((dir.clone(), format!("{name}/{}", relative(&dir, file))))
}

/// A template file's documents (split at `---`): each whose `kind:` is written out, with its lines and
/// values. Its name is the one written out — or, if a template makes it, the file's (until rendered).
pub fn templates(text: &str, file: &str) -> Vec<Doc> {
    let stem = file.rsplit('/').next().unwrap_or(file);
    let stem = stem.rsplit_once('.').map_or(stem, |(s, _)| s);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..=lines.len() {
        if i < lines.len() && !lines[i].trim_end().starts_with("---") {
            continue;
        }
        let part = &lines[start..i];
        let first = start as u64;
        start = i + 1;
        // Its top-level `kind:` and `apiVersion:`, as written (not a template's)
        let top = |key: &str| {
            part.iter().enumerate().find_map(|(n, l)| {
                let v = l.strip_prefix(key)?.strip_prefix(':')?.trim();
                let v = v.split(" #").next().unwrap_or(v).trim().trim_matches(['"', '\'']);
                Some((n as u64, v.to_string()))
            })
        };
        let Some((at, kind)) = top("kind").filter(|(_, k)| !k.is_empty() && !k.contains("{{")) else {
            continue;
        };
        let api_version = top("apiVersion").map(|(_, v)| v).filter(|v| !v.contains("{{")).unwrap_or_default();
        let fields = crate::fields::fields(part, first);
        let written = |key: &str| {
            fields.iter().find(|f| {
                !f.templated && matches!(f.path.as_slice(), [crate::fields::Seg::Key(m), crate::fields::Seg::Key(k)] if m == "metadata" && k == key)
            })
        };
        let name = written("name")
            .and_then(|f| f.value.as_str().map(str::to_string))
            .unwrap_or_else(|| stem.to_string());
        let namespace = written("namespace").and_then(|f| f.value.as_str().map(str::to_string));
        out.push(Doc {
            lines: first..first + part.len() as u64,
            at: first + at,
            api_version,
            kind,
            name,
            namespace,
            generated: false,
            via: Via::Building,
            fields,
        });
        if out.len() == MAX_DOCS {
            break;
        }
    }
    out
}

/// A release of a chart: its name and namespace.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Release {
    pub name: String,
    pub namespace: String,
}

/// Whether a `helm.sh/chart` label is chart `name`'s (`<name>-<version>`).
fn of_chart(label: &str, name: &str) -> bool {
    label
        .strip_prefix(name)
        .and_then(|r| r.strip_prefix('-'))
        .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit() || c == 'v'))
}

/// The releases of chart `name`, from the labels of the cluster's objects (their `helm.sh/chart` label,
/// release, namespace).
pub fn releases<'a>(
    labeled: impl IntoIterator<Item = &'a (String, String, String)>,
    name: &str,
) -> Vec<Release> {
    let mut out: Vec<Release> = labeled
        .into_iter()
        .filter(|(chart, release, _)| of_chart(chart, name) && !release.is_empty())
        .map(|(_, release, ns)| Release { name: release.clone(), namespace: ns.clone() })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// An object `helm template` put out, and the template it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendered {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    pub source: String,
    /// A hook's (`helm.sh/hook`: `test`, `pre-install`, …): the cluster has it while it runs, if at all.
    pub hook: Option<String>,
}

/// `helm template`'s output.
pub fn rendered(output: &str) -> Vec<Rendered> {
    let mut out = Vec::new();
    let mut source = String::new();
    let mut body = String::new();
    let mut flush = |source: &str, body: &mut String| {
        if let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(body) {
            let s = |v: &serde_yaml::Value| v.as_str().map(str::to_string);
            if let (Some(api_version), Some(kind), Some(name)) =
                (s(&v["apiVersion"]), s(&v["kind"]), s(&v["metadata"]["name"]))
            {
                out.push(Rendered {
                    api_version,
                    kind,
                    name,
                    namespace: s(&v["metadata"]["namespace"]),
                    source: source.to_string(),
                    hook: s(&v["metadata"]["annotations"]["helm.sh/hook"]),
                });
            }
        }
        body.clear();
    };
    for line in output.lines() {
        if line.trim_end() == "---" {
            flush(&source, &mut body);
            source.clear();
        } else if let Some(s) = line.strip_prefix("# Source: ") {
            source = s.trim().to_string();
        } else {
            body.push_str(line);
            body.push('\n');
        }
    }
    flush(&source, &mut body);
    out
}

/// What each of a template file's documents (their kinds, in order) became: the k-th rendered object of
/// its kind from that source — none, if its template didn't put it out (with the values rendered).
pub fn became<'a>(rendered: &'a [Rendered], source: &str, kinds: &[&str]) -> Vec<Option<&'a Rendered>> {
    let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
    kinds
        .iter()
        .map(|kind| {
            let k = seen.entry(kind).or_default();
            let r = rendered.iter().filter(|r| r.source == source && r.kind == *kind).nth(*k);
            *k += 1;
            r
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_template_s_chart_and_source_a_subchart_s_through_its_parent() {
        let charts: BTreeMap<String, String> =
            [("/r/charts/shop", "shop"), ("/r/charts/shop/charts/cache", "cache"), ("/r/other", "other")]
                .into_iter()
                .map(|(d, n)| (d.to_string(), n.to_string()))
                .collect();
        assert_eq!(
            source_of(&charts, "/r/charts/shop/templates/deployment.yaml"),
            Some(("/r/charts/shop".into(), "shop/templates/deployment.yaml".into()))
        );
        assert_eq!(
            source_of(&charts, "/r/charts/shop/charts/cache/templates/sts.yaml"),
            Some(("/r/charts/shop".into(), "shop/charts/cache/templates/sts.yaml".into()))
        );
        assert_eq!(source_of(&charts, "/r/apps/templates/x.yaml"), None);
        assert!(
            is_template("/r/charts/shop/templates/tests/test.yaml")
                && !is_template("/r/charts/shop/templates/_helpers.tpl")
        );
        assert_eq!(chart_name("apiVersion: v2\nname: shop\nversion: 0.1.0\n").as_deref(), Some("shop"));
    }

    #[test]
    fn a_template_file_s_documents_by_their_written_kinds() {
        let text = "\
{{- if .Values.serviceAccount.create -}}
apiVersion: v1
kind: ServiceAccount
metadata:
  name: {{ include \"shop.serviceAccountName\" . }}
{{- end }}
---
apiVersion: {{ include \"ingress.apiVersion\" . }}
kind: Ingress
metadata:
  name: web
  namespace: edge
---
kind: {{ .Values.kind }}
";
        let d = templates(text, "/r/charts/shop/templates/misc.yaml");
        assert_eq!(d.len(), 2);
        assert_eq!(
            (d[0].at, &d[0].kind[..], &d[0].name[..], &d[0].api_version[..]),
            (2, "ServiceAccount", "misc", "v1")
        );
        assert_eq!(d[0].lines, 0..6);
        assert_eq!(
            (d[1].at, &d[1].name[..], d[1].namespace.as_deref(), &d[1].api_version[..]),
            (8, "web", Some("edge"), "")
        );
    }

    #[test]
    fn releases_by_their_chart_label() {
        let labeled = [
            ("shop-0.1.0".to_string(), "shop-helm".to_string(), "shop".to_string()),
            ("shop-0.1.0".into(), "shop-helm".into(), "shop".into()),
            ("shop-0.2.0".into(), "shop-canary".into(), "canary".into()),
            ("shop-cache-1.0.0".into(), "cache".into(), "shop".into()),
        ];
        let r = releases(&labeled, "shop");
        assert_eq!(r.iter().map(|r| &r.name[..]).collect::<Vec<_>>(), ["shop-canary", "shop-helm"]);
        assert_eq!(releases(&labeled, "shop-cache")[0].name, "cache");
    }

    #[test]
    fn what_helm_rendered_leads_back_to_its_template_s_documents() {
        let output = "\
---
# Source: shop/templates/service.yaml
apiVersion: v1
kind: Service
metadata:
  name: shop-helm
---
# Source: shop/templates/misc.yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: shop-helm-a
---
# Source: shop/templates/misc.yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: shop-helm-b
  namespace: other
---
# Source: shop/templates/tests/test-connection.yaml
apiVersion: v1
kind: Pod
metadata:
  name: shop-helm-test
  annotations:
    \"helm.sh/hook\": test
";
        let r = rendered(output);
        assert_eq!(r.len(), 4);
        assert_eq!(r[3].hook.as_deref(), Some("test"));
        let b = became(&r, "shop/templates/misc.yaml", &["ConfigMap", "Secret", "ConfigMap", "ConfigMap"]);
        let names: Vec<Option<&str>> = b.iter().map(|x| x.map(|r| r.name.as_str())).collect();
        assert_eq!(names, [Some("shop-helm-a"), None, Some("shop-helm-b"), None]);
        assert_eq!(b[2].unwrap().namespace.as_deref(), Some("other"));
    }
}
