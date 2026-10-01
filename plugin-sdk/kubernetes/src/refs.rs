//! What a manifest's lines name in its namespace — the objects it needs there: a ConfigMap (and its key) or a
//! Secret its containers read, its ServiceAccount, a claim it mounts, a pull secret, an Ingress's Service, an
//! HPA's target. Written as a block (`name: app-settings` under `configMapKeyRef:`) or a flow mapping
//! (`configMapKeyRef: {name: app-settings, key: mode}`). Ones marked `optional: true`, and ones a template
//! writes, are left out: nothing's wrong if they're missing, or the name isn't known.

use serde_json::Value;

use crate::fields::{Field, Seg};

/// An object a line names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ref {
    /// The line the name is on.
    pub line: u64,
    pub api_version: String,
    pub kind: String,
    pub name: String,
    /// A ConfigMap's key it reads.
    pub key: Option<String>,
}

/// Where a reference is: the key it's under, the kind it names, and the key its name is in.
const HOLDERS: [(&str, &str, &str); 9] = [
    ("configMapKeyRef", "ConfigMap", "name"),
    ("configMapRef", "ConfigMap", "name"),
    ("configMap", "ConfigMap", "name"),
    ("secretKeyRef", "Secret", "name"),
    ("secretRef", "Secret", "name"),
    ("secret", "Secret", "secretName"),
    ("persistentVolumeClaim", "PersistentVolumeClaim", "claimName"),
    ("scaleTargetRef", "", "name"),
    ("service", "Service", "name"),
];

/// The value at `prop` of the holder at `at`: in its flow mapping, or on a line of its own under it — and
/// that line, if it's one.
fn prop(fields: &[Field], at: &[Seg], prop: &str) -> Option<(Value, Option<u64>)> {
    let mut path = at.to_vec();
    path.push(Seg::Key(prop.into()));
    if let Some(f) = fields.iter().find(|f| f.path == path && !f.nested) {
        return (!f.templated).then(|| (f.value.clone(), Some(f.line)));
    }
    let f = fields.iter().find(|f| f.path == at && f.value.is_object())?;
    Some((f.value.get(prop)?.clone(), None))
}

/// The objects a document's lines name.
pub fn refs(fields: &[Field]) -> Vec<Ref> {
    let mut out: Vec<Ref> = Vec::new();
    // Each holder once: from the line of its name, or of its flow mapping
    let mut holders: Vec<(Vec<Seg>, &str, &str)> = Vec::new();
    for f in fields {
        for (holder, kind, name) in HOLDERS {
            let flow = f.value.is_object() && f.path.last() == Some(&Seg::Key(holder.into()));
            let block = f.path.len() >= 2
                && f.path[f.path.len() - 1] == Seg::Key(name.into())
                && f.path[f.path.len() - 2] == Seg::Key(holder.into());
            let at = if flow { f.path.clone() } else if block { f.path[..f.path.len() - 1].to_vec() } else { continue };
            // (an Ingress's `service` is its backend's)
            if holder == "service" && at.len() < 2 || holder == "service" && at[at.len() - 2] != Seg::Key("backend".into())
            {
                continue;
            }
            if !holders.iter().any(|(a, ..)| *a == at) {
                holders.push((at, kind, name));
            }
        }
        // A ServiceAccount, a pull secret: the name itself
        let last = f.path.last();
        if !f.templated
            && let Some(name) = f.value.as_str()
        {
            if last == Some(&Seg::Key("serviceAccountName".into())) {
                out.push(Ref { line: f.line, api_version: "v1".into(), kind: "ServiceAccount".into(), name: name.into(), key: None });
            }
            let n = f.path.len();
            if n >= 3
                && f.path[n - 3] == Seg::Key("imagePullSecrets".into())
                && matches!(f.path[n - 2], Seg::Item(_))
                && last == Some(&Seg::Key("name".into()))
            {
                out.push(Ref { line: f.line, api_version: "v1".into(), kind: "Secret".into(), name: name.into(), key: None });
            }
        }
    }
    for (at, kind, name_key) in holders {
        let Some((Value::String(name), line)) = prop(fields, &at, name_key) else { continue };
        if prop(fields, &at, "optional").is_some_and(|(v, _)| v == true) {
            continue;
        }
        let line = line.or_else(|| fields.iter().find(|f| f.path == at).map(|f| f.line));
        let Some(line) = line else { continue };
        // An HPA's target says its own kind
        let (api_version, kind) = if kind.is_empty() {
            let Some((Value::String(k), _)) = prop(fields, &at, "kind") else { continue };
            let av = match prop(fields, &at, "apiVersion") {
                Some((Value::String(av), _)) => av,
                _ => "apps/v1".into(),
            };
            (av, k)
        } else {
            ("v1".to_string(), kind.to_string())
        };
        let key = match kind.as_str() {
            "ConfigMap" if at.last() == Some(&Seg::Key("configMapKeyRef".into())) => match prop(fields, &at, "key") {
                Some((Value::String(k), _)) => Some(k),
                _ => None,
            },
            _ => None,
        };
        out.push(Ref { line, api_version, kind, name, key });
    }
    out.sort_by_key(|r| r.line);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str) -> Vec<(u64, String, String, Option<String>)> {
        let lines: Vec<&str> = text.lines().collect();
        refs(&crate::fields::fields(&lines, 0)).into_iter().map(|r| (r.line, r.kind, r.name, r.key)).collect()
    }

    #[test]
    fn what_a_workload_s_lines_name_block_or_flow() {
        let text = "\
spec:
  template:
    spec:
      serviceAccountName: worker
      imagePullSecrets:
        - name: registry
      containers:
        - name: app
          env:
            - name: MODE
              valueFrom:
                configMapKeyRef:
                  name: app-settings
                  key: mode
            - name: TOKEN
              valueFrom:
                secretKeyRef: {name: app-token, key: token}
            - name: MAYBE
              valueFrom:
                configMapKeyRef: {name: extra, key: x, optional: true}
          envFrom:
            - configMapRef:
                name: shared
      volumes:
        - name: data
          persistentVolumeClaim:
            claimName: data-pvc
        - name: certs
          secret:
            secretName: tls-certs
        - name: tpl
          configMap:
            name: {{ .Values.cm }}
";
        assert_eq!(
            read(text),
            [
                (3, "ServiceAccount".into(), "worker".into(), None),
                (5, "Secret".into(), "registry".into(), None),
                (12, "ConfigMap".into(), "app-settings".into(), Some("mode".into())),
                (16, "Secret".into(), "app-token".into(), None),
                (22, "ConfigMap".into(), "shared".into(), None),
                (26, "PersistentVolumeClaim".into(), "data-pvc".into(), None),
                (29, "Secret".into(), "tls-certs".into(), None),
            ],
            "the optional one and the templated one left out"
        );
    }

    #[test]
    fn an_ingress_s_service_and_an_hpa_s_target() {
        let ingress = "\
spec:
  rules:
    - http:
        paths:
          - path: /
            backend:
              service:
                name: web
                port: {number: 80}
";
        assert_eq!(read(ingress), [(7, "Service".into(), "web".into(), None)]);
        let hpa = "spec:\n  scaleTargetRef: {apiVersion: apps/v1, kind: Deployment, name: web}\n";
        let lines: Vec<&str> = hpa.lines().collect();
        let r = refs(&crate::fields::fields(&lines, 0));
        assert_eq!((r[0].line, r[0].api_version.as_str(), r[0].kind.as_str(), r[0].name.as_str()), (1, "apps/v1", "Deployment", "web"));
    }
}
