//! The Kubernetes objects a buffer holds: its YAML documents (split at `---`), each with the lines it takes
//! and the line its `kind:` is on — where its state is written. A document that's a template (`{{ }}`), a
//! `List`, or has no name is left out.

use std::ops::Range;

/// The most documents read from one buffer.
const MAX_DOCS: usize = 200;

#[derive(Clone, Debug, PartialEq)]
pub struct Doc {
    /// Its lines (from 0).
    pub lines: Range<u64>,
    /// The line of its top-level `kind:`.
    pub at: u64,
    pub api_version: String,
    pub kind: String,
    pub name: String,
    /// As written (none: the context's namespace, for a namespaced kind).
    pub namespace: Option<String>,
    /// Made by a kustomization's generator (its `name:` line), not written out.
    pub generated: bool,
    /// Through Kustomize: what the cluster has of it is what a build made of it.
    pub via: Via,
    /// The values its lines write (to put the cluster's beside them where it differs).
    pub fields: Vec<crate::fields::Field>,
}

/// How a document's object is found in the cluster.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Via {
    /// As written.
    #[default]
    Itself,
    /// A kustomization takes it in; it's being built.
    Building,
    /// As this build made it: a Kustomize root (its folder's name), or a Helm release (its name).
    Built(String),
    /// A kustomization takes it in, but nothing could be built: why.
    Unbuilt(String),
}

/// A line that ends one document: `---` (maybe with a comment), or `...`.
fn separator(line: &str) -> bool {
    let l = line.trim_end();
    l == "..." || l == "---" || l.strip_prefix("---").is_some_and(|r| r.starts_with([' ', '\t']) && r.trim_start().starts_with('#'))
}

pub fn docs(text: &str) -> Vec<Doc> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..=lines.len() {
        if i < lines.len() && !separator(lines[i]) {
            continue;
        }
        if let Some(d) = doc(&lines[start..i], start as u64) {
            out.push(d);
            if out.len() == MAX_DOCS {
                break;
            }
        }
        start = i + 1;
    }
    out
}

fn doc(lines: &[&str], first: u64) -> Option<Doc> {
    let body = lines.join("\n");
    if body.contains("{{") {
        return None;
    }
    let v: serde_yaml::Value = serde_yaml::from_str(&body).ok()?;
    let s = |v: &serde_yaml::Value| v.as_str().map(str::to_string);
    let (api_version, kind) = (s(&v["apiVersion"])?, s(&v["kind"])?);
    let name = s(&v["metadata"]["name"])?;
    if kind == "List" || kind.ends_with("List") && v.get("items").is_some() {
        return None;
    }
    let at = lines.iter().position(|l| l.starts_with("kind:"))? as u64;
    Some(Doc {
        lines: first..first + lines.len() as u64,
        at: first + at,
        api_version,
        kind,
        name,
        namespace: s(&v["metadata"]["namespace"]),
        generated: false,
        via: Via::Itself,
        fields: crate::fields::fields(lines, first),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents_with_their_lines() {
        let text = "\
# the web app
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: prod
---
apiVersion: v1
kind: Service
metadata:
  name: web
--- # a template: left out
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ .Release.Name }}
---
kind: Nameless
apiVersion: v1
";
        let d = docs(text);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].at, &d[0].kind[..], &d[0].name[..], d[0].namespace.as_deref()), (2, "Deployment", "web", Some("prod")));
        assert_eq!(d[0].lines, 0..6);
        assert_eq!((d[1].at, &d[1].api_version[..], d[1].namespace.as_deref()), (8, "v1", None));
        assert_eq!(d[1].lines, 7..11);
        assert!(docs("not: [yaml").is_empty());
    }
}
