//! The language a code block names, in tarae's words, so the markdown's fence colours it (web-plan D3.3). Four
//! conventions seen on the fixtures, and the common one: `<code data-lang="rust">` (Zola's giallo — the Rust
//! blog), `<pre class="rust …">` (rustdoc), `<pre class="brush: http">` (MDN), GitHub's `highlight-source-rust`
//! on the block's wrapper, `language-rust` / `lang-rust` (Prism, highlight.js, CommonMark renderers).

use super::tree::{Id, Tree};

/// The language of a `<pre>`: on it, on its `<code>`, or on the wrapper GitHub puts around it.
pub fn of(t: &Tree, pre: Id) -> Option<String> {
    let code = t.elements(pre).find(|&c| t.name(c) == Some("code"));
    let mut places = vec![pre];
    places.extend(code);
    for &p in &places {
        if let Some(l) = t.attr(p, "data-lang").or_else(|| t.attr(p, "data-language")) {
            return named(l);
        }
    }
    for &p in &places {
        let class = t.attr(p, "class").unwrap_or_default();
        if let Some(after) = class.split("brush:").nth(1) {
            return after.split_whitespace().next().and_then(named);
        }
        for c in class.split_whitespace() {
            if let Some(l) = c.strip_prefix("language-").or_else(|| c.strip_prefix("lang-")) {
                return named(l);
            }
        }
    }
    // GitHub: `<div class="highlight highlight-source-rust"><pre>`
    for p in t.up(pre, 2) {
        let class = t.attr(p, "class").unwrap_or_default();
        for c in class.split_whitespace() {
            if let Some(l) = c.strip_prefix("highlight-source-").or_else(|| c.strip_prefix("highlight-text-"))
            {
                return named(l.split('-').next().unwrap_or(l));
            }
        }
    }
    // rustdoc: a bare class that is a language's name — only a name tarae knows (a class is anything)
    places.iter().find_map(|&p| {
        t.attr(p, "class").unwrap_or_default().split_whitespace().find_map(|c| known(c).map(String::from))
    })
}

/// A name a page gave a block's language, as tarae names it: its own name for one tarae knows, else the name as
/// given if it looks like one (`http`: no colours yet, but it says what the block is); none for plain text.
pub fn named(given: &str) -> Option<String> {
    let l = given.trim().to_ascii_lowercase();
    if matches!(
        l.as_str(),
        "" | "plain" | "plaintext" | "text" | "txt" | "none" | "nohighlight" | "no-highlight"
    ) {
        return None;
    }
    if let Some(k) = known(&l) {
        return Some(k.into());
    }
    let word = l.len() <= 24
        && l.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '#' | '-' | '_' | '.'));
    word.then_some(l)
}

/// tarae's language names (runtime/packs), and the other names pages give them.
fn known(l: &str) -> Option<&'static str> {
    Some(match l {
        "rust" | "rs" => "rust",
        "bash" | "sh" | "shell" | "zsh" | "console" | "shellsession" | "shell-session" | "terminal" => "bash",
        "c" | "h" => "c",
        "cpp" | "c++" | "cxx" | "cc" | "hpp" => "cpp",
        "c-sharp" | "csharp" | "cs" | "c#" => "c-sharp",
        "cmake" => "cmake",
        "css" => "css",
        "dart" => "dart",
        "diff" | "patch" => "diff",
        "dockerfile" | "docker" => "dockerfile",
        "elixir" | "ex" | "exs" => "elixir",
        "fish" => "fish",
        "go" | "golang" => "go",
        "graphql" | "gql" => "graphql",
        "haskell" | "hs" => "haskell",
        "hcl" | "terraform" | "tf" => "hcl",
        "html" | "htm" | "xhtml" => "html",
        "ini" => "ini",
        "java" => "java",
        "javascript" | "js" | "mjs" | "cjs" | "node" => "javascript",
        "jsx" => "jsx",
        "json" | "jsonc" | "json5" => "json",
        "just" | "justfile" => "just",
        "kdl" => "kdl",
        "kotlin" | "kt" | "kts" => "kotlin",
        "lua" => "lua",
        "make" | "makefile" | "mk" => "make",
        "markdown" | "md" => "markdown",
        "nix" => "nix",
        "nu" | "nushell" => "nu",
        "ocaml" | "ml" => "ocaml",
        "php" => "php",
        "protobuf" | "proto" => "protobuf",
        "python" | "py" | "python3" | "py3" => "python",
        "ruby" | "rb" => "ruby",
        "scala" => "scala",
        "scss" => "scss",
        "sql" => "sql",
        "svelte" => "svelte",
        "swift" => "swift",
        "toml" => "toml",
        "typescript" | "ts" => "typescript",
        "tsx" => "tsx",
        "vue" => "vue",
        "xml" | "svg" => "xml",
        "yaml" | "yml" => "yaml",
        "zig" => "zig",
        _ => return None,
    })
}
