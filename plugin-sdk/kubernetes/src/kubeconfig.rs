//! The kubeconfig, understood: its current context's cluster and user — what connecting takes, what files it
//! names that are still to read (relative ones from its folder), and the credential program to run (EKS's
//! `aws eks get-token`) if that's how it signs in.

use std::collections::HashMap;

use base64::Engine;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Kubeconfig {
    #[serde(rename = "current-context", default)]
    current: String,
    #[serde(default)]
    contexts: Vec<Named<ContextEntry>>,
    #[serde(default)]
    clusters: Vec<Named<ClusterEntry>>,
    #[serde(default)]
    users: Vec<Named<UserEntry>>,
}

#[derive(Deserialize)]
struct Named<T> {
    name: String,
    #[serde(alias = "context", alias = "cluster", alias = "user")]
    it: T,
}

#[derive(Deserialize)]
struct ContextEntry {
    cluster: String,
    user: String,
    namespace: Option<String>,
}

#[derive(Deserialize)]
struct ClusterEntry {
    server: String,
    #[serde(rename = "certificate-authority-data")]
    ca_data: Option<String>,
    #[serde(rename = "certificate-authority")]
    ca_file: Option<String>,
    /// The name to check the server's certificate against, if not the server's host.
    #[serde(rename = "tls-server-name")]
    server_name: Option<String>,
    #[serde(rename = "insecure-skip-tls-verify", default)]
    insecure: bool,
    #[serde(rename = "proxy-url")]
    proxy: Option<String>,
}

#[derive(Deserialize)]
struct UserEntry {
    #[serde(rename = "client-certificate-data")]
    cert_data: Option<String>,
    #[serde(rename = "client-key-data")]
    key_data: Option<String>,
    #[serde(rename = "client-certificate")]
    cert_file: Option<String>,
    #[serde(rename = "client-key")]
    key_file: Option<String>,
    token: Option<String>,
    #[serde(rename = "tokenFile")]
    token_file: Option<String>,
    exec: Option<Exec>,
    #[serde(rename = "auth-provider")]
    auth_provider: Option<Value>,
}

/// A program that hands out credentials (`aws eks get-token`), run for a token and again before it
/// expires.
#[derive(Clone, Debug, Deserialize)]
pub struct Exec {
    pub command: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub env: Option<Vec<ExecEnv>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ExecEnv {
    pub name: String,
    pub value: String,
}

/// The credential programs it may run (for EKS), and the variables they may be given.
pub const EXEC_PROGRAMS: [&str; 1] = ["aws"];
pub const EXEC_ENV: [&str; 5] =
    ["AWS_PROFILE", "AWS_REGION", "AWS_DEFAULT_REGION", "AWS_CONFIG_FILE", "AWS_SHARED_CREDENTIALS_FILE"];

/// A kubeconfig on its way to being a connection: files it names still to read.
pub struct Loading {
    pub context: String,
    pub namespace: Option<String>,
    pub server: String,
    pub server_name: Option<String>,
    pub ca: Option<Vec<u8>>,
    pub cert: Option<Vec<u8>>,
    pub key: Option<Vec<u8>>,
    pub token: Option<String>,
    /// Request id → which file it is.
    pub files: HashMap<u64, &'static str>,
    /// Waiting for the credential program's token.
    pub exec_pending: bool,
}

/// What the kubeconfig says to do: (what's known, files to read as (which, path), the program to run).
pub struct Plan {
    pub loading: Loading,
    pub files: Vec<(&'static str, String)>,
    pub exec: Option<Exec>,
}

fn b64(s: &str) -> Result<Vec<u8>, String> {
    base64::engine::general_purpose::STANDARD.decode(s.trim()).map_err(|e| format!("kubeconfig base64: {e}"))
}

/// A path the kubeconfig names: from its folder, unless absolute (or from ~).
fn beside(kubeconfig: &str, path: &str) -> String {
    if path.starts_with('/') || path.starts_with("~/") {
        return path.to_string();
    }
    let dir = kubeconfig.rsplit_once('/').map_or(".", |(d, _)| d);
    format!("{dir}/{path}")
}

/// Reads the kubeconfig at `path` (its text): its current context, and what signing in takes.
pub fn plan(text: &[u8], path: &str) -> Result<Plan, String> {
    let k: Kubeconfig = serde_yaml::from_slice(text).map_err(|e| format!("the kubeconfig: {e}"))?;
    let Some(ctx) = k.contexts.iter().find(|c| c.name == k.current) else {
        return Err(if k.current.is_empty() {
            "the kubeconfig has no current context (kubectl config use-context …)".into()
        } else {
            format!("no context {} in the kubeconfig", k.current)
        });
    };
    let (Some(cluster), Some(user)) = (
        k.clusters.iter().find(|c| c.name == ctx.it.cluster),
        k.users.iter().find(|u| u.name == ctx.it.user),
    ) else {
        return Err(format!("context {}: its cluster or user is missing", k.current));
    };
    let (cluster, user) = (&cluster.it, &user.it);
    if user.auth_provider.is_some() {
        return Err("auth-provider (the old gcp/oidc plugins): not supported — use exec credentials".into());
    }
    if cluster.insecure {
        return Err("insecure-skip-tls-verify: not supported — the server's certificate is always checked (give the kubeconfig its CA)".into());
    }
    if cluster.proxy.is_some() {
        return Err("proxy-url: not supported yet".into());
    }
    let exec = match &user.exec {
        Some(e) => {
            // `aws` (EKS), as a name or a path: the one on PATH
            let program = e.command.rsplit('/').next().unwrap_or("");
            if !EXEC_PROGRAMS.contains(&program) {
                return Err(format!("exec credentials with {}: only aws (EKS) for now", e.command));
            }
            if let Some(v) = e.env.iter().flatten().find(|v| !EXEC_ENV.contains(&v.name.as_str())) {
                return Err(format!(
                    "exec credentials: the variable {} isn't passed on (only {})",
                    v.name,
                    EXEC_ENV.join(", ")
                ));
            }
            Some(Exec { command: program.to_string(), ..e.clone() })
        }
        None => None,
    };
    let decode = |d: &Option<String>| d.as_deref().map(b64).transpose();
    let (ca, cert, key) = (decode(&cluster.ca_data)?, decode(&user.cert_data)?, decode(&user.key_data)?);
    let named = [
        (&cluster.ca_file, "ca"),
        (&user.cert_file, "cert"),
        (&user.key_file, "key"),
        (if user.token.is_none() { &user.token_file } else { &None }, "token"),
    ];
    let files = named.into_iter().filter_map(|(p, what)| p.as_ref().map(|p| (what, beside(path, p)))).collect();
    Ok(Plan {
        loading: Loading {
            context: k.current.clone(),
            namespace: ctx.it.namespace.clone(),
            server: cluster.server.clone(),
            server_name: cluster.server_name.clone(),
            ca,
            cert,
            key,
            token: user.token.clone(),
            files: HashMap::new(),
            exec_pending: exec.is_some(),
        },
        files,
        exec,
    })
}

/// A server URL's host, port and path: `https://k:6443`, `https://[::1]:6443`, `https://r.example/k8s/clusters/c-1`.
pub fn server(url: &str) -> Result<(String, u64, String), String> {
    let rest = url.strip_prefix("https://").ok_or_else(|| format!("{url}: only https servers"))?;
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let bad = || format!("{url}: can't make out its host and port");
    let (host, port) = match authority.strip_prefix('[') {
        // IPv6
        Some(v6) => {
            let (h, after) = v6.split_once(']').ok_or_else(bad)?;
            match after {
                "" => (h, 443),
                p => (h, p.strip_prefix(':').and_then(|p| p.parse().ok()).ok_or_else(bad)?),
            }
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse().map_err(|_| bad())?),
            None => (authority, 443),
        },
    };
    if host.is_empty() {
        return Err(bad());
    }
    let path = path.trim_end_matches('/');
    Ok((host.to_string(), port, if path.is_empty() { String::new() } else { format!("/{path}") }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_with_ports_ipv6_and_paths() {
        let ok = |url: &str| server(url).unwrap();
        assert_eq!(ok("https://k:6443"), ("k".into(), 6443, String::new()));
        assert_eq!(ok("https://k"), ("k".into(), 443, String::new()));
        assert_eq!(ok("https://[::1]:6443/"), ("::1".into(), 6443, String::new()));
        assert_eq!(ok("https://[fd00::1]"), ("fd00::1".into(), 443, String::new()));
        assert_eq!(
            ok("https://r.example/k8s/clusters/c-1/"),
            ("r.example".into(), 443, "/k8s/clusters/c-1".into())
        );
        assert!(server("http://k:8080").is_err());
        assert!(server("https://[::1:6443").is_err());
        assert!(server("https://k:port").is_err());
    }

    const EKS: &str = r#"
apiVersion: v1
current-context: eks
contexts: [{name: eks, context: {cluster: c, user: u, namespace: prod}}]
clusters: [{name: c, cluster: {server: "https://ABC.gr7.ap-northeast-2.eks.amazonaws.com", certificate-authority: ca.crt}}]
users:
- name: u
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1beta1
      command: /opt/homebrew/bin/aws
      args: [--region, ap-northeast-2, eks, get-token, --cluster-name, prod]
      env: [{name: AWS_PROFILE, value: work}]
"#;

    #[test]
    fn eks_signs_in_with_aws_and_files_are_read_from_the_kubeconfigs_folder() {
        let p = plan(EKS.as_bytes(), "~/.kube/config").unwrap_or_else(|e| panic!("{e}"));
        let e = p.exec.unwrap();
        assert_eq!((e.command.as_str(), e.env.unwrap()[0].value.as_str()), ("aws", "work"), "the aws on PATH");
        assert!(p.loading.exec_pending);
        assert_eq!(p.loading.namespace.as_deref(), Some("prod"));
        assert_eq!(p.files, [("ca", "~/.kube/ca.crt".to_string())]);
        let bad = |from: &str, to: &str| plan(EKS.replace(from, to).as_bytes(), "~/.kube/config").err().unwrap();
        assert!(bad("/opt/homebrew/bin/aws", "gke-gcloud-auth-plugin").contains("only aws"));
        assert!(bad("AWS_PROFILE", "AWS_SECRET_ACCESS_KEY").contains("AWS_SECRET_ACCESS_KEY isn't passed on"));
        assert!(bad("current-context: eks", "current-context: nope").contains("no context nope"));
    }
}
