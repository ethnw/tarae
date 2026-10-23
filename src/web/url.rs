//! A link's address resolved against the page's (RFC 3986 §5.2, as the HTML spec has it: ASCII tabs and newlines
//! taken out, spaces at the ends trimmed). Only what a reader needs: no host is checked here — where a link may go is
//! the core's to decide when it's followed.

/// `href` against `base` (an absolute URL): the address it names, or none where it isn't one to follow
/// (`javascript:`, `data:`, nothing at all).
pub fn resolve(base: &str, href: &str) -> Option<String> {
    let href: String = href.trim().chars().filter(|c| !matches!(c, '\t' | '\n' | '\r')).collect();
    if href.is_empty() {
        return None;
    }
    if let Some(scheme) = scheme(&href) {
        return match scheme.to_ascii_lowercase().as_str() {
            "javascript" | "data" | "vbscript" | "blob" => None,
            s => Some(format!("{s}{}", &href[scheme.len()..])),
        };
    }
    let b = Parts::of(base)?;
    let out = if let Some(rest) = href.strip_prefix("//") {
        format!("{}://{rest}", b.scheme)
    } else if href.starts_with('#') {
        format!("{}{href}", b.without_fragment())
    } else if href.starts_with('?') {
        format!("{}://{}{}{href}", b.scheme, b.authority, b.path)
    } else if href.starts_with('/') {
        let (path, tail) = split_tail(&href);
        format!("{}://{}{}{tail}", b.scheme, b.authority, dots(path))
    } else {
        let (path, tail) = split_tail(&href);
        let dir = match b.path.rfind('/') {
            Some(i) => &b.path[..=i],
            None => "/",
        };
        format!("{}://{}{}{tail}", b.scheme, b.authority, dots(&format!("{dir}{path}")))
    };
    Some(out)
}

/// A URL's host (its authority without user and port), lower-cased.
pub fn host(url: &str) -> Option<String> {
    let p = Parts::of(url)?;
    let auth = p.authority.rsplit('@').next().unwrap_or_default();
    let host = match auth.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => auth.split(':').next().unwrap_or_default(),
    };
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// A URL as a markdown link's destination: spaces escaped, and `<…>` around one whose brackets don't pair.
pub fn in_markdown(url: &str) -> String {
    let url = url.replace(' ', "%20");
    let mut depth = 0i32;
    let paired = url.chars().all(|c| {
        depth += match c {
            '(' => 1,
            ')' => -1,
            _ => 0,
        };
        depth >= 0
    }) && depth == 0;
    if paired && !url.contains(['<', '>']) {
        url
    } else {
        format!("<{}>", url.replace('<', "%3C").replace('>', "%3E"))
    }
}

fn scheme(s: &str) -> Option<&str> {
    let end = s.find(':')?;
    let name = &s[..end];
    let mut chars = name.chars();
    (chars.next()?.is_ascii_alphabetic()
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
    .then_some(name)
}

struct Parts<'a> {
    scheme: &'a str,
    authority: &'a str,
    path: &'a str,
    whole: &'a str,
}

impl<'a> Parts<'a> {
    fn of(url: &'a str) -> Option<Parts<'a>> {
        let scheme = scheme(url)?;
        let rest = url[scheme.len() + 1..].strip_prefix("//")?;
        let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let after = &rest[auth_end..];
        let path_end = after.find(['?', '#']).unwrap_or(after.len());
        let path = &after[..path_end];
        Some(Parts {
            scheme,
            authority: &rest[..auth_end],
            path: if path.is_empty() { "/" } else { path },
            whole: url,
        })
    }

    fn without_fragment(&self) -> &'a str {
        self.whole.split('#').next().unwrap_or(self.whole)
    }
}

/// A path and what follows it (`?query#fragment`).
fn split_tail(s: &str) -> (&str, &str) {
    let i = s.find(['?', '#']).unwrap_or(s.len());
    (&s[..i], &s[i..])
}

/// RFC 3986 §5.2.4: `.` and `..` segments taken out.
fn dots(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let segs: Vec<&str> = path.split('/').skip(1).collect();
    for (i, seg) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        match *seg {
            "." => {
                if last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}
