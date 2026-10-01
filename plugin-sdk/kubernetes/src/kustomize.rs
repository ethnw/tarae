//! Kustomize, as far as the plugin needs it: the project's kustomizations (what each takes in — files,
//! other kustomizations, components — and the ConfigMaps and Secrets it generates), which of them are
//! built on their own (the roots: no other takes them in), and which roots a file ends up in. A root is
//! rendered by `kustomize build`, through a kustomization of the plugin's own that takes it in and asks for
//! each object's origin (`buildMetadata: [originAnnotations]`) — so an object in the output leads back to
//! the file, and the document, it came from, whatever the overlays did to its name and namespace.

use std::collections::BTreeMap;

/// A kustomization: its folder, what it takes in (absolute: files or folders), and what it generates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Kz {
    pub dir: String,
    pub takes: Vec<String>,
    /// (kind, name, the line its `name:` is on) — a generated ConfigMap or Secret.
    pub generates: Vec<(String, String, u64)>,
}

/// `a/b/../c` → `a/c`.
fn clean(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." if !out.is_empty() => {}
            ".." if out.len() > 1 => {
                out.pop();
            }
            p => out.push(p),
        }
    }
    let s = out.join("/");
    if s.is_empty() { "/".into() } else { s }
}

/// `path` from folder `dir` (absolute unless already).
pub fn join(dir: &str, path: &str) -> String {
    if path.starts_with('/') { clean(path) } else { clean(&format!("{dir}/{path}")) }
}

/// The folder a path is in.
pub fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or(".", |(d, _)| if d.is_empty() { "/" } else { d })
}

/// `to` as a path from folder `from` (both absolute): `../../repo/apps/web`.
pub fn relative(from: &str, to: &str) -> String {
    let a: Vec<&str> = from.split('/').filter(|s| !s.is_empty()).collect();
    let b: Vec<&str> = to.split('/').filter(|s| !s.is_empty()).collect();
    let same = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let mut parts: Vec<&str> = vec![".."; a.len() - same];
    parts.extend(&b[same..]);
    if parts.is_empty() { ".".into() } else { parts.join("/") }
}

/// A kustomization's text, read (at `path`).
pub fn parse(path: &str, text: &str) -> Kz {
    let dir = parent(path).to_string();
    let v: serde_yaml::Value = serde_yaml::from_str(text).unwrap_or(serde_yaml::Value::Null);
    let list = |key: &str| -> Vec<String> {
        v.get(key)
            .and_then(|l| l.as_sequence())
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str())
            // (a remote one — `github.com/…?ref=` — isn't the project's)
            .filter(|x| !x.contains("://") && !x.starts_with("github.com/"))
            .map(|x| join(&dir, x))
            .collect()
    };
    let mut takes = list("resources");
    takes.extend(list("bases"));
    takes.extend(list("components"));
    // Generated: each `name:` under configMapGenerator / secretGenerator, and its line
    let mut generates = Vec::new();
    let mut under: Option<&str> = None;
    for (i, line) in text.lines().enumerate() {
        if !line.starts_with([' ', '\t', '-']) {
            under = match line.split(':').next().map(str::trim) {
                Some("configMapGenerator") => Some("ConfigMap"),
                Some("secretGenerator") => Some("Secret"),
                _ => None,
            };
            continue;
        }
        let Some(kind) = under else { continue };
        let item = line.trim_start().trim_start_matches("- ").trim_start();
        if let Some(name) = item.strip_prefix("name:") {
            let name = name.trim().trim_matches(['"', '\'']);
            if !name.is_empty() {
                generates.push((kind.to_string(), name.to_string(), i as u64));
            }
        }
    }
    Kz { dir, takes, generates }
}

/// The project's kustomizations, by folder.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    pub kzs: BTreeMap<String, Kz>,
}

impl Tree {
    /// The roots: kustomizations no other takes in.
    pub fn roots(&self) -> Vec<&str> {
        self.kzs
            .keys()
            .filter(|dir| !self.kzs.values().any(|k| k.takes.iter().any(|t| t == *dir)))
            .map(String::as_str)
            .collect()
    }

    /// Whether building `root` takes in `file` (a file it or a kustomization it takes in lists, or one of
    /// their own kustomization files).
    fn reaches(&self, root: &str, file: &str, seen: &mut Vec<String>) -> bool {
        if seen.iter().any(|s| s == root) {
            return false;
        }
        seen.push(root.to_string());
        let Some(k) = self.kzs.get(root) else { return false };
        if parent(file) == root && is_kustomization(file) {
            return true;
        }
        k.takes.iter().any(|t| t == file || (self.kzs.contains_key(t) && self.reaches(t, file, seen)))
    }

    /// The roots whose build takes in `file`.
    pub fn roots_of(&self, file: &str) -> Vec<String> {
        self.roots().into_iter().filter(|r| self.reaches(r, file, &mut Vec::new())).map(str::to_string).collect()
    }
}

pub fn is_kustomization(path: &str) -> bool {
    matches!(path.rsplit('/').next(), Some("kustomization.yaml" | "kustomization.yml" | "Kustomization"))
}

/// The plugin's own kustomization in folder `wrap` that builds `root` and says where each object came from.
pub fn wrapper(wrap: &str, root: &str) -> String {
    format!("resources:\n  - {}\nbuildMetadata: [originAnnotations]\n", relative(wrap, root))
}

/// An object `kustomize build` put out: what it is, and where it came from (absolute) — the file it was
/// in, or the kustomization whose generator made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    pub api_version: String,
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    pub path: Option<String>,
    pub configured_in: Option<String>,
}

/// `kustomize build`'s output for the wrapper in folder `wrap`.
pub fn built(output: &str, wrap: &str) -> Vec<Built> {
    let mut out = Vec::new();
    for doc in output.split("\n---\n") {
        let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(doc) else { continue };
        let s = |v: &serde_yaml::Value| v.as_str().map(str::to_string);
        let (Some(api_version), Some(kind), Some(name)) = (s(&v["apiVersion"]), s(&v["kind"]), s(&v["metadata"]["name"])) else {
            continue;
        };
        let origin = v["metadata"]["annotations"]["config.kubernetes.io/origin"]
            .as_str()
            .and_then(|o| serde_yaml::from_str::<serde_yaml::Value>(o).ok())
            .unwrap_or(serde_yaml::Value::Null);
        out.push(Built {
            api_version,
            kind,
            name,
            namespace: s(&v["metadata"]["namespace"]),
            path: s(&origin["path"]).map(|p| join(wrap, &p)),
            configured_in: s(&origin["configuredIn"]).map(|p| join(wrap, &p)),
        });
    }
    out
}

/// Which built object a document of `file` became: of its kind, from that file, named as it was — or
/// with an overlay's prefix and suffix around the name (the closest, if several).
pub fn became<'a>(built: &'a [Built], file: &str, kind: &str, name: &str, generated: bool) -> Option<&'a Built> {
    let from = |b: &&Built| {
        let at = if generated { b.configured_in.as_deref() } else { b.path.as_deref() };
        b.kind == kind && at == Some(file)
    };
    let mine: Vec<&Built> = built.iter().filter(from).collect();
    mine.iter().find(|b| b.name == name).or_else(|| {
        mine.iter().filter(|b| b.name.contains(name)).min_by_key(|b| b.name.len())
    }).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_joined_cleaned_and_made_relative() {
        assert_eq!(join("/r/apps/web/overlays/prod", "../../base"), "/r/apps/web/base");
        assert_eq!(join("/r/a", "./x.yaml"), "/r/a/x.yaml");
        assert_eq!(relative("/state/plugins/kubernetes/kz/1", "/r/apps/web"), "../../../../../r/apps/web");
        assert_eq!(relative("/r/a", "/r/a/b"), "b");
        assert_eq!(parent("/r/a/kustomization.yaml"), "/r/a");
    }

    #[test]
    fn kustomizations_read_their_roots_found_and_which_take_a_file() {
        let base = parse(
            "/r/apps/web/base/kustomization.yaml",
            "resources:\n  - deployment.yaml\n  - https://example.com/x.yaml\nconfigMapGenerator:\n  - name: web-settings\n    literals: [A=1]\nsecretGenerator:\n- name: \"web-creds\"\n",
        );
        assert_eq!(base.takes, ["/r/apps/web/base/deployment.yaml"]);
        assert_eq!(
            base.generates,
            [("ConfigMap".into(), "web-settings".into(), 4), ("Secret".into(), "web-creds".into(), 7)]
        );
        let prod = parse("/r/apps/web/overlays/prod/kustomization.yaml", "namespace: prod\nresources:\n  - ../../base\n");
        let dev = parse("/r/apps/web/overlays/dev/kustomization.yaml", "bases: [../../base]\n");
        let tree = Tree { kzs: [base, prod, dev].into_iter().map(|k| (k.dir.clone(), k)).collect() };
        assert_eq!(tree.roots(), ["/r/apps/web/overlays/dev", "/r/apps/web/overlays/prod"]);
        assert_eq!(tree.roots_of("/r/apps/web/base/deployment.yaml").len(), 2);
        assert_eq!(tree.roots_of("/r/apps/web/base/kustomization.yaml").len(), 2, "its generators");
        assert_eq!(tree.roots_of("/r/apps/web/overlays/prod/kustomization.yaml"), ["/r/apps/web/overlays/prod"]);
        assert!(tree.roots_of("/r/other.yaml").is_empty());
    }

    #[test]
    fn what_was_built_leads_back_to_its_file_and_document() {
        // (kustomize build's output for a wrapper in /s/kz/1, the project in /r)
        let output = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  annotations:\n    config.kubernetes.io/origin: |\n      configuredIn: ../../../r/apps/web/base/kustomization.yaml\n      configuredBy:\n        apiVersion: builtin\n        kind: ConfigMapGenerator\n  name: prod-web-settings-hf678c7m2b\n  namespace: prod\n---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  annotations:\n    config.kubernetes.io/origin: |\n      path: ../../../r/apps/web/base/deployment.yaml\n  name: prod-web\n  namespace: prod\n";
        let b = built(output, "/s/kz/1");
        assert_eq!(b.len(), 2);
        let d = became(&b, "/r/apps/web/base/deployment.yaml", "Deployment", "web", false).unwrap();
        assert_eq!((d.name.as_str(), d.namespace.as_deref()), ("prod-web", Some("prod")));
        let c = became(&b, "/r/apps/web/base/kustomization.yaml", "ConfigMap", "web-settings", true).unwrap();
        assert_eq!(c.name, "prod-web-settings-hf678c7m2b");
        assert!(became(&b, "/r/apps/web/base/deployment.yaml", "Service", "web", false).is_none());
    }
}
