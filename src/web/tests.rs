//! The reader on saved pages (tests/web/pages, each with its address and date in pages.toml): a snapshot per page
//! (tests/web/snapshots; `TARAE_BLESS=1` writes them anew), then what each page must keep, checked by name. Then the
//! pieces on small HTML of their own, the limits, the property's loop (`check`), `:web`'s keys and the times.

use std::path::{Path, PathBuf};

use super::reader::{self, DEPTH_MAX, PAGE_MAX, Page, Prefer, Read, TEXT_MAX};
use super::tree::{self, Tree};
use super::{charset, check, convert, lang, url};

struct Fixture {
    name: String,
    url: String,
    content_type: Option<String>,
}

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/web")
}

fn fixtures() -> Vec<Fixture> {
    let toml = std::fs::read_to_string(dir().join("pages.toml")).unwrap();
    let table: toml::Table = toml.parse().unwrap();
    let s = |p: &toml::Value, k: &str| p.get(k).and_then(|v| v.as_str()).map(String::from);
    table["page"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| Fixture {
            name: s(p, "name").unwrap(),
            url: s(p, "url").unwrap(),
            content_type: s(p, "content-type"),
        })
        .collect()
}

fn bytes(name: &str) -> Vec<u8> {
    std::fs::read(dir().join(format!("pages/{name}.html"))).unwrap()
}

fn page(name: &str) -> Page {
    let f = fixtures().into_iter().find(|f| f.name == name).unwrap();
    read(&f)
}

fn read(f: &Fixture) -> Page {
    reader::read(&bytes(&f.name), &f.url, f.content_type.as_deref(), Prefer::Guess)
}

/// What a snapshot holds: how it read, its counts, then the text.
fn snapshot(p: &Page) -> String {
    let mut s = format!(
        "title: {}\nread: {}\ncharset: {}\nlinks: {}\nanchors: {}\n",
        p.title,
        p.read.word(),
        p.charset,
        p.links.len(),
        p.anchors.len()
    );
    if let Some(r) = &p.refresh {
        s.push_str(&format!("refresh: {}s {}\n", r.secs, r.url));
    }
    if let Some(said) = &p.said {
        s.push_str(&format!("said: {said}\n"));
    }
    s.push_str("---\n");
    s.push_str(&p.text);
    s
}

#[test]
fn every_page_reads_as_its_snapshot() {
    let bless = std::env::var_os("TARAE_BLESS").is_some();
    let mut differ = Vec::new();
    for f in fixtures() {
        let got = snapshot(&read(&f));
        let at = dir().join(format!("snapshots/{}.md", f.name));
        if bless {
            std::fs::write(&at, &got).unwrap();
        } else if std::fs::read_to_string(&at).ok().as_deref() != Some(got.as_str()) {
            differ.push(f.name);
        }
    }
    assert!(
        differ.is_empty(),
        "read differently from their snapshots (TARAE_BLESS=1 to take them): {differ:?}"
    );
}

/// Each page, read each way, holds the reader's property: its links where the text has them, its anchors on its
/// lines, the limits.
#[test]
fn every_page_read_every_way_holds_the_property() {
    for f in fixtures() {
        for prefer in [Prefer::Guess, Prefer::Main, Prefer::Article, Prefer::Page] {
            check::page(&bytes(&f.name), f.content_type.as_deref(), prefer);
        }
        let p = read(&f);
        assert!(p.links.iter().all(|l| l.url.contains("://") || l.url.starts_with("mailto:")), "{}", f.name);
    }
}

/// Headings outside code blocks.
fn headings(md: &str) -> Vec<&str> {
    let mut fenced = false;
    md.lines()
        .filter(|l| {
            if l.starts_with("```") {
                fenced = !fenced;
            }
            !fenced && l.starts_with('#') && l.trim_start_matches('#').starts_with(' ')
        })
        .collect()
}

#[test]
fn docs_rs_keeps_every_heading_of_its_main() {
    let p = page("docsrs");
    assert_eq!(p.read, Read::Main);
    let html = String::from_utf8(bytes("docsrs")).unwrap();
    let t = parsed(&html);
    let main = t.named("main").next().unwrap();
    let there =
        t.walk(main).filter(|&n| matches!(t.name(n), Some("h1" | "h2" | "h3" | "h4" | "h5" | "h6"))).count();
    let h = headings(&p.text);
    assert_eq!(h.len(), there, "{h:#?}");
    assert_eq!(there, 221);
    // (the method's name linked to its own section: words, not a link)
    assert!(
        h.iter().any(|h| h.starts_with("#### pub fn remove<R>(&mut self, char_range: R) where")),
        "{h:#?}"
    );
    // A heading's own `§` link goes; its anchor stays, on its line
    assert!(!p.text.contains("[§]"));
    let line = p.anchors.iter().find(|a| a.name == "method.remove").unwrap().line;
    assert!(p.text.lines().nth(line).unwrap().contains("pub fn remove"), "{line}");
}

#[test]
fn the_issue_keeps_its_comments_and_leaves_out_what_github_hid() {
    let p = page("issue");
    assert_eq!(p.read, Read::Main);
    assert!(p.text.contains("\n# web: read a saved page into a buffer #12\n"), "{}", p.text);
    for words in [
        "The web reader turns a saved page into text tarae can show",
        "A page's bytes are anyone's: the reader must never panic on them",
        "Agreed. The reader goes into the core, so Claude's `read` always has it.",
        "> never panic on them, whatever they hold",
        "added a commit that references this issue",
    ] {
        assert!(p.text.contains(words), "{words}");
    }
    assert!(p.text.contains("```rust\nlet page = web::read(&bytes, url, None, Prefer::Guess);\n"));
    // The two GitHub hid, as it shows them: who, when, why — not their words
    assert_eq!(p.text.matches("[on Oct 23, 2026](#issuecomment-").count(), 4);
    assert_eq!(p.text.matches(" · Hidden as off-topic").count(), 2);
    for gone in [
        "hidden comment's words",
        "Copy link",
        "Status: Open",
        "Subscribe",
        "preloaded_records",
        "M12 1C5",
        "Sign in",
        "© 2026 GitHub",
        "Pull requests",
    ] {
        assert!(!p.text.contains(gone), "{gone} left out:\n{}", p.text);
    }
}

#[test]
fn mdn_keeps_its_first_paragraph() {
    let p = page("mdn");
    assert_eq!(p.read, Read::Main);
    assert!(
        p.text.contains("response header allows website administrators to control resources the user agent")
    );
    assert!(p.text.contains("```http\n"));
}

#[test]
fn the_blogs_code_says_rust() {
    let p = page("blog");
    assert_eq!(p.read, Read::Article);
    assert_eq!(p.text.matches("```rust\n").count(), 6, "{}", p.text);
}

#[test]
fn the_spa_says_it_needs_javascript() {
    let p = page("spa");
    assert_eq!(p.read, Read::Page);
    assert!(p.said.as_deref().unwrap().starts_with("needs JavaScript"));
    assert!(p.text.contains("You need to enable JavaScript to run this app."));
    assert!(p.text.contains("tarae doesn't run a page's scripts"));
}

#[test]
fn the_blogs_old_address_refreshes_to_its_new_one() {
    let r = page("blog-redirect").refresh.unwrap();
    assert_eq!((r.secs, r.url.as_str()), (0, "https://blog.rust-lang.org/2024/11/28/Rust-1.83.0/"));
    // rustdoc's, relative to the page
    let r = page("refresh").refresh.unwrap();
    assert_eq!((r.secs, r.url.as_str()), (0, "https://docs.rs/ropey/latest/ropey/struct.Rope.html"));
}

/// The HTML spec's reading of a refresh's `content` (the reviewer's: `url = x` was taken for a relative address
/// named `url = x`).
#[test]
fn a_refresh_reads_as_the_html_spec_reads_it() {
    let refresh = |content: &str| {
        let html = format!(
            "<html><head><meta http-equiv=Refresh content=\"{content}\"></head><body>x</body></html>"
        );
        reader::read(html.as_bytes(), "https://a.org/b/c.html", None, Prefer::Guess)
            .refresh
            .map(|r| (r.secs, r.url))
    };
    let to = |secs, url: &str| Some((secs, format!("https://a.org/b/{url}")));
    let cases = [
        ("0;url=x", to(0, "x")),
        ("0; url = x", to(0, "x")),
        ("0 ; URL\t=\t'x'", to(0, "x")),
        ("0;url='x'#gone", to(0, "x")),
        ("0; url=&quot;x&quot;y", to(0, "x")),
        ("5, x", to(5, "x")),
        ("3 x", to(3, "x")),
        ("0.5; url=x", to(0, "x")),
        (".5;url=x", to(0, "x")),
        ("0;urlx", to(0, "urlx")),
        ("0; u=x", to(0, "u=x")),
        ("0;'x'", to(0, "x")),
        ("0", None),
        ("0;", None),
        ("x", None),
        ("0x; url=y", None),
        ("99999999999999999999999; url=x", Some((u64::MAX, "https://a.org/b/x".into()))),
    ];
    for (content, want) in cases {
        assert_eq!(refresh(content), want, "{content}");
    }
}

/// The charset code on a page of its own: EUC-KR said by the header, or by the page's `<meta>` without one.
#[test]
fn an_euc_kr_page_reads_in_korean() {
    let body = "타래는 실타래를 뛰는 묶음입니다. ".repeat(40);
    let html = format!(
        "<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=EUC-KR\">\
         <title>타래 안내</title></head><body><main><p>{body}</p></main></body></html>"
    );
    let (euc_kr, _, lossy) = encoding_rs::EUC_KR.encode(&html);
    assert!(!lossy);
    for content_type in [Some("text/html; charset=euc-kr"), None] {
        let p = reader::read(&euc_kr, "https://a.kr/", content_type, Prefer::Guess);
        assert_eq!(
            (p.charset, p.title.as_str(), p.read),
            ("EUC-KR", "타래 안내", Read::Main),
            "{content_type:?}"
        );
        assert!(p.text.starts_with("타래는 실타래를") && !p.text.contains('\u{fffd}'), "{}", p.text);
    }
}

#[test]
fn links_resolve_against_the_pages_base() {
    // php.net's short address serves the manual's page; its <base> says where it is
    let p = reader::read(&bytes("base"), "https://php.net/strlen", None, Prefer::Guess);
    assert!(p.links.iter().any(|l| l.url == "https://www.php.net/manual/en/function.mb-strlen.php"));
    assert!(!p.links.iter().any(|l| l.url.starts_with("https://php.net/")));
}

#[test]
fn the_tables_are_pipe_tables() {
    let p = page("table");
    assert_eq!(p.read, Read::Main);
    assert!(p.title.starts_with("ISO 639"));
    let rows = p.text.lines().filter(|l| l.starts_with("| ")).count();
    assert!(rows > 70, "{rows}");
    assert!(p.text.contains("\n| Arabic | — | `Ar` | `=927` | `ar` | `ar` |\n"));
    // The navboxes (role=navigation) and the notice laid out in a table (role=presentation) aren't tables
    assert!(!p.text.contains("| v t e"));
}

#[test]
fn the_reader_can_be_told_which_part() {
    let url = "https://docs.rs/ropey/latest/ropey/struct.Rope.html";
    let main = reader::read(&bytes("docsrs"), url, None, Prefer::Guess);
    let page = reader::read(&bytes("docsrs"), url, None, Prefer::Page);
    assert_eq!((main.read, page.read), (Read::Main, Read::Page));
    assert!(page.text.len() > main.text.len());
}

/// No `<main>`: the manual is the article (it holds the page's titled heading, `strlen`), not the user notes below it
/// — longer, denser text, but not what the page is.
#[test]
fn the_php_manual_is_the_article_not_its_user_notes() {
    let p = page("base");
    assert_eq!(p.read, Read::Article);
    assert!(p.text.starts_with("# strlen\n"), "{}", p.text);
    for kept in ["### Description", "function **strlen**", "### Examples", "### See Also", "[mb_strlen()]"] {
        assert!(p.text.contains(kept), "{kept}: {}", p.text);
    }
    for gone in ["User Contributed Notes", "rm dot nasir", "years ago", "Found A Problem"] {
        assert!(!p.text.contains(gone), "{gone}: {}", p.text);
    }
}

// ── The pieces ──

/// A paragraph of about 300 characters, three commas in each sentence, its word first.
fn para(word: &str) -> String {
    format!("<p>{}</p>", format!("{word} said, in a few words, one, two, of the text ").repeat(6))
}

fn guessed(html: &str, title: &str) -> Page {
    let page = format!("<html><head><title>{title}</title></head><body>{html}</body></html>");
    reader::read(page.as_bytes(), "https://x.org/", Some("text/html"), Prefer::Guess)
}

#[test]
fn the_pick_takes_main_then_a_lone_article_then_role_main_then_the_densest_block() {
    let menu = "<ul>".to_string()
        + &"<li><a href=/x>a link to somewhere else entirely</a></li>".repeat(30)
        + "</ul>";
    let p =
        guessed(&format!("<article>{}</article><main>{}</main>", para("alpha"), para("beta").repeat(2)), "t");
    assert_eq!(p.read, Read::Main);
    assert!(p.text.contains("beta said") && !p.text.contains("alpha"), "{}", p.text);
    let p =
        guessed(&format!("<div>{}</div><article>{}</article>", para("alpha"), para("beta").repeat(2)), "t");
    assert_eq!(p.read, Read::Article);
    assert!(p.text.contains("beta said") && !p.text.contains("alpha"), "{}", p.text);
    // Two articles: neither is the page's
    let two = format!(
        "<article>{}</article><article>{}</article>",
        para("alpha").repeat(3),
        para("beta").repeat(3)
    );
    let p = guessed(&format!("{two}<div role=main>{}</div>", para("gamma").repeat(3)), "t");
    assert_eq!(p.read, Read::Main);
    assert!(p.text.contains("gamma said") && !p.text.contains("alpha"), "{}", p.text);
    let p = guessed(&format!("<div>{menu}</div><div>{}</div>", para("delta").repeat(4)), "t");
    assert_eq!(p.read, Read::Article);
    assert!(p.text.contains("delta said") && !p.text.contains("a link"), "{}", p.text);
    // Too little to be a block: the page
    let p = guessed(&format!("<div>{menu}</div><div>{}</div>", para("delta")), "t");
    assert_eq!(p.read, Read::Page);
}

/// The block the page's title heads is the article when the densest block lies apart from it (php.net's manual and
/// its user notes); a heading the title doesn't name, or a dense block inside the titled one, leaves the densest.
#[test]
fn the_titled_block_wins_over_a_denser_one_apart_from_it() {
    let doc = format!("<div id=doc><h1>strlen</h1>{}</div>", para("manual").repeat(3));
    let notes = format!("<div id=notes>{}</div>", para("note").repeat(5));
    let p = guessed(&format!("{doc}{notes}"), "PHP: strlen - Manual");
    assert_eq!(p.read, Read::Article);
    assert!(p.text.starts_with("# strlen\n") && !p.text.contains("note said"), "{}", p.text);
    let p = guessed(&format!("{doc}{notes}"), "PHP: Manual");
    assert!(p.text.contains("note said") && !p.text.contains("manual said"), "{}", p.text);
    // (the title over one block of paragraphs: that block)
    let p = guessed(
        &format!("<div id=all><h1>strlen</h1>{}</div><div>{}</div>", para("one").repeat(5), para("two")),
        "strlen",
    );
    assert!(p.text.starts_with("# strlen\n") && !p.text.contains("two said"), "{}", p.text);
}

fn parsed(html: &str) -> Tree {
    tree::parse(html, DEPTH_MAX).tree
}

fn md(html: &str) -> convert::Out {
    let t = parsed(html);
    convert::markdown(&t, t.body().unwrap(), "https://example.com/a/b.html")
}

#[test]
fn markers_hug_their_words_and_empty_ones_leave_nothing() {
    let o = md("<p>a <b> bold </b>word, <em></em><i class=icon></i>and <a href=x> </a>none</p>");
    assert_eq!(o.text, "a **bold** word, and none\n");
    let o = md("<p><b>Example <strong>strlen()</strong> <i>x</i></b></p>");
    assert_eq!(o.text, "**Example strlen() *x***\n");
}

#[test]
fn links_are_written_where_they_stand_and_listed() {
    let o =
        md(r##"<p>See <a href="../c.html#x">the <code>c</code> page</a> and <a href="#top">top</a>.</p>"##);
    assert_eq!(o.text, "See [the `c` page](https://example.com/c.html#x) and [top](#top).\n");
    assert_eq!(o.links[0].url, "https://example.com/c.html#x");
    assert_eq!(&o.text[o.links[0].from..o.links[0].to], "[the `c` page](https://example.com/c.html#x)");
    assert_eq!(o.links[1].url, "https://example.com/a/b.html#top");
}

#[test]
fn lists_nest_and_quotes_keep_their_marks() {
    let o = md("<ul><li>one<ul><li>two</li></ul></li><li><p>three</p><p>more</p></li></ul>\
                <ol start=3><li>c</li><li>d</li></ol><blockquote><p>q</p><p>r</p></blockquote>");
    assert_eq!(o.text, "- one\n  - two\n- three\n\n  more\n\n3. c\n4. d\n\n> q\n>\n> r\n");
}

#[test]
fn code_blocks_are_fences_with_their_language() {
    let o = md("<ul><li>x<pre class=\"language-rs\"><code>let a = 1;\n\nlet b = 2;\n</code></pre></li></ul>");
    assert_eq!(o.text, "- x\n\n  ```rust\n  let a = 1;\n\n  let b = 2;\n  ```\n");
    let o = md("<pre>a ``` b</pre>");
    assert_eq!(o.text, "````\na ``` b\n````\n");
}

#[test]
fn the_four_conventions_name_the_language() {
    let lang_of = |html: &str| {
        let t = parsed(html);
        let pre = t.named("pre").next().unwrap();
        lang::of(&t, pre)
    };
    let cases = [
        (r#"<pre><code data-lang="rust">x</code></pre>"#, Some("rust")),
        (r#"<pre class="rust rust-example-rendered"><code>x</code></pre>"#, Some("rust")),
        (r#"<pre class="brush: js notranslate">x</pre>"#, Some("javascript")),
        (r#"<pre class="brush: http notranslate">x</pre>"#, Some("http")),
        (r#"<div class="highlight highlight-source-shell"><pre>x</pre></div>"#, Some("bash")),
        (r#"<div class="highlight highlight-text-html-basic"><pre>x</pre></div>"#, Some("html")),
        (r#"<pre><code class="language-py">x</code></pre>"#, Some("python")),
        (r#"<pre class="notranslate">x</pre>"#, None),
        (r#"<pre><code data-lang="plain">x</code></pre>"#, None),
    ];
    for (html, want) in cases {
        assert_eq!(lang_of(html).as_deref(), want, "{html}");
    }
}

#[test]
fn a_tables_cells_keep_their_links_and_layout_tables_are_blocks() {
    let o = md(
        "<table><tr><th>Name</th><th>What</th></tr><tr><td><a href=/n>n|m</a></td><td colspan=2>x</td></tr></table>",
    );
    assert_eq!(
        o.text,
        "| Name | What |  |\n| --- | --- | --- |\n| [n\\|m](https://example.com/n) | x |  |\n"
    );
    let l = &o.links[0];
    assert_eq!(&o.text[l.from..l.to], "[n\\|m](https://example.com/n)");
    let o = md("<table><tr><td>only</td></tr><tr><td>one column</td></tr></table>");
    assert_eq!(o.text, "only\none column\n");
}

#[test]
fn the_pages_chrome_goes_an_articles_header_stays() {
    let o = md(
        "<header>Site</header><nav>Menu</nav><article><header><h1 id=t>Title</h1></header><p>Text</p>\
                <footer>by me</footer></article><footer>© site</footer><div hidden>no</div><p aria-hidden=true>no</p>",
    );
    assert_eq!(o.text, "# Title\n\nText\n\nby me\n");
    assert_eq!(o.anchors, vec![convert::Anchor { name: "t".into(), line: 0 }]);
}

#[test]
fn a_headings_own_anchor_link_goes() {
    let o = md(r##"<h2 id="x"><a class="doc-anchor" href="#x">§</a>Editing <a href="#y">ops</a></h2>"##);
    assert_eq!(o.text, "## Editing [ops](#y)\n");
}

#[test]
fn addresses_resolve_as_rfc_3986_says() {
    let base = "https://a.com/b/c/d.html?q#f";
    let cases = [
        ("g", "https://a.com/b/c/g"),
        ("./g/", "https://a.com/b/c/g/"),
        ("../g", "https://a.com/b/g"),
        ("../../../g", "https://a.com/g"),
        ("/g?x#y", "https://a.com/g?x#y"),
        ("//b.org/x", "https://b.org/x"),
        ("?y", "https://a.com/b/c/d.html?y"),
        ("#s", "https://a.com/b/c/d.html?q#s"),
        ("HTTP://X.org/", "http://X.org/"),
        (" mailto:a@b.c ", "mailto:a@b.c"),
    ];
    for (href, want) in cases {
        assert_eq!(url::resolve(base, href).as_deref(), Some(want), "{href}");
    }
    assert_eq!(url::resolve(base, "javascript:void(0)"), None);
    assert_eq!(url::resolve(base, ""), None);
    assert_eq!(url::host("https://u@Docs.RS:443/x").as_deref(), Some("docs.rs"));
    assert_eq!(url::in_markdown("https://w.org/a_(b)"), "https://w.org/a_(b)");
    assert_eq!(url::in_markdown("https://w.org/a b)"), "<https://w.org/a%20b)>");
}

#[test]
fn the_charset_comes_from_the_bom_the_header_or_the_meta() {
    let (korean, _, _) = encoding_rs::EUC_KR.encode("<p>한글</p>");
    let page = [b"<meta charset=euc-kr>".as_slice(), &korean].concat();
    assert_eq!(charset::decode(&page, None).0, "<meta charset=euc-kr><p>한글</p>");
    let page =
        [b"<meta http-equiv=Content-Type content='text/html; charset=EUC-KR'>".as_slice(), &korean].concat();
    assert_eq!(charset::decode(&page, None).1.name(), "EUC-KR");
    assert_eq!(charset::decode(&korean, Some("text/html; charset=\"euc-kr\"")).1.name(), "EUC-KR");
    // The header wins over the meta; a BOM over both
    assert_eq!(charset::decode(&page, Some("text/html; charset=utf-8")).1.name(), "UTF-8");
    let bom = [b"\xEF\xBB\xBF<p>x".as_slice()].concat();
    assert_eq!(charset::decode(&bom, Some("text/html; charset=euc-kr")).1.name(), "UTF-8");
    assert_eq!(charset::decode(b"<p>x", None).1.name(), "UTF-8");
}

#[test]
fn what_isnt_html_comes_as_it_is_or_not_at_all() {
    let p = reader::read(b"{\"a\": 1}", "https://x.org/a.json", Some("application/json"), Prefer::Guess);
    assert_eq!((p.read, p.language.as_deref(), p.text.as_str()), (Read::Text, Some("json"), "{\"a\": 1}"));
    let p = reader::read(b"%PDF-1.7\0", "https://x.org/a.pdf", Some("application/pdf"), Prefer::Guess);
    assert_eq!((p.read, p.text.as_str()), (Read::None, ""));
    assert_eq!(p.said.as_deref(), Some("application/pdf, not a page tarae reads"));
    let p = reader::read(b"just words", "file:///a/notes", None, Prefer::Guess);
    assert_eq!((p.read, p.text.as_str()), (Read::Text, "just words"));
}

/// A saved file with no content type (the reviewer's): what its bytes are is said — a PDF, a picture by its kind
/// (`media::kind`), else bytes with a NUL —, nothing read.
#[test]
fn bytes_with_no_type_say_what_they_are() {
    let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".as_slice();
    let cases: [(&[u8], &str); 4] = [
        (b"%PDF-1.7\n%\xe2\xe3\n1 0 obj", "a PDF, not a page tarae reads"),
        (png, "a PNG picture, not a page tarae reads"),
        (b"\xff\xd8\xff\xe0\0\x10JFIF\0", "a JPEG picture, not a page tarae reads"),
        (b"PK\x03\x04\x14\0\0\0\x08\0", "binary data, not a page tarae reads"),
    ];
    for (b, said) in cases {
        let p = reader::read(b, "file:///a/saved", None, Prefer::Guess);
        assert_eq!((p.read, p.text.as_str(), p.said.as_deref()), (Read::None, "", Some(said)));
    }
}

// ── The limits ──

#[test]
fn a_text_past_the_limit_is_cut_at_a_line_and_said() {
    let big = format!("<p>{}</p>", "word ".repeat(TEXT_MAX / 5 + 10)).replace("word word ", "word<br>word ");
    let p = reader::read(big.as_bytes(), "https://x.org/", Some("text/html"), Prefer::Page);
    assert!(p.text.len() <= TEXT_MAX && p.text.ends_with('\n'));
    assert_eq!(p.said.as_deref(), Some("its text cut at 2.0 MB"));
}

#[test]
fn a_page_past_the_limit_is_read_to_it_and_said() {
    let big = format!(
        "<html><body><main>{}</main></body></html>",
        "<p>words in a paragraph</p>".repeat(PAGE_MAX / 20)
    );
    assert!(big.len() > PAGE_MAX);
    let p = reader::read(big.as_bytes(), "https://x.org/", None, Prefer::Guess);
    let said = p.said.unwrap();
    assert!(said.starts_with("only its first 8.0 MB read · its text cut at"), "{said}");
    assert_eq!(p.read, Read::Main);
}

/// A run of `n` elements nested in one another, `open` each (the converter's deepest walks: a list's item, a quote,
/// a link, emphasis, a table's cell), a word at the bottom.
fn nested(n: usize, open: &str, close: &str) -> String {
    format!("<html><body>{}deep{}</body></html>", open.repeat(n), close.repeat(n))
}

/// Nesting past `DEPTH_MAX` is left out and said, on a worker's stack (the main thread's default, the smallest a
/// job gets) — the converter's walk is recursive; the tree's walks and the pick are loops.
#[test]
fn nesting_past_the_limit_is_left_out_and_said_within_a_workers_stack() {
    let shapes = [
        ("<ul><li>", "</li></ul>"),
        ("<blockquote>", "</blockquote>"),
        ("<div><b><i>", "</i></b></div>"),
        ("<table><tr><td>", "</td></tr></table>"),
        ("<span>", "</span>"),
    ];
    let worker = std::thread::Builder::new().name("web-deep".into()).stack_size(2 << 20);
    worker
        .spawn(move || {
            for (open, close) in shapes {
                for prefer in [Prefer::Guess, Prefer::Article, Prefer::Page] {
                    let page = nested(20_000, open, close);
                    let t = std::time::Instant::now();
                    let p = reader::read(page.as_bytes(), "https://x.org/", None, prefer);
                    // (unbounded, html5ever took 4–17 s on these at 20,000)
                    assert!(t.elapsed() < std::time::Duration::from_secs(2), "{open}: {:?}", t.elapsed());
                    let said = p.said.unwrap_or_default();
                    assert!(said.contains(&format!("nested past {DEPTH_MAX} levels")), "{open}: {said}");
                    assert!(!p.text.contains("deep"), "{open}");
                }
                // Within it, read whole
                // (a table's level is four elements: its body the parser's)
                let p = reader::read(
                    nested(DEPTH_MAX / 5, open, close).as_bytes(),
                    "https://x.org/",
                    None,
                    Prefer::Page,
                );
                assert!(p.text.contains("deep") && p.said.is_none(), "{open}: {:?} {:?}", p.said, p.text);
            }
        })
        .unwrap()
        .join()
        .expect("read within a worker's stack");
}

/// What a page's few bytes could make many of — columns, a table's padding, lines' prefixes — stays within the
/// text's limit, and quick.
#[test]
fn what_a_page_could_multiply_stays_within_the_limits() {
    let wide = format!(
        "<table><tr>{}</tr>{}</table>",
        "<td colspan=1000000>x</td>".repeat(5000),
        "<tr><td>y</td></tr>".repeat(200_000)
    );
    // (each nested within the limit: past it the parse stops, and these would test nothing)
    let prefixed = format!("{}{}", "<ul><li>".repeat(200), "<br>".repeat(1_000_000));
    let words = "word ".repeat(400_000);
    let mains = format!("{}{words}{}", "<main><p>".repeat(200), "</p></main>".repeat(200));
    let noscripts = format!("<script></script>{}{words}", "<noscript>".repeat(400));
    let headings = "<section id=s><h1 hidden>h</h1></section>".repeat(100_000);
    for page in [wide, prefixed, mains, noscripts, headings] {
        let t = std::time::Instant::now();
        check::page(page.as_bytes(), Some("text/html"), Prefer::Guess);
        let took = t.elapsed();
        assert!(took < std::time::Duration::from_secs(20), "{took:?} for {}", &page[..40]);
        let p = reader::read(page.as_bytes(), "https://x.org/", Some("text/html"), Prefer::Guess);
        assert!(!p.said.unwrap_or_default().contains("nested past"), "{}", &page[..40]);
    }
}

/// The property's loop (the fuzz target's in-tree twin, `fuzz/parsers` `web`): the small fixtures and HTML of its
/// own, mutated by fixed seeds — the same every run. By hand, longer: `TARAE_WEB_MUTATE_SECS=60` runs it that long
/// (where libFuzzer can't be had), saying how many it read.
#[test]
fn mutated_pages_hold_the_property() {
    let secs = std::env::var("TARAE_WEB_MUTATE_SECS").ok().and_then(|s| s.parse::<u64>().ok());
    let until = secs.map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s));
    use crate::mutate::{Rng, bytes as mutate};
    let mut seeds: Vec<Vec<u8>> = ["blog-redirect", "spa", "issue"].iter().map(|n| bytes(n)).collect();
    seeds.extend(seed_pages());
    let words: &[&[u8]] = &[
        b"<main>",
        b"</main>",
        b"<pre class=\"brush: rust\">",
        b"<table><tr><td colspan=9>",
        b"</td>",
        b"<li>",
        b"<a href=\"#x\">",
        b"</a>",
        b"<h2 id=x>",
        b"<meta http-equiv=refresh content=\"0; url = ",
        b"<base href=\"//b.org/\">",
        b"<noscript>",
        b"<script>",
        b"<b>",
        b"&amp;",
        b"\xea\xb0",
        b"\0",
        b"<!--",
    ];
    let mut rng = Rng(0x7eb_5eed_0000_0001);
    let mut k = 0;
    while until.map_or(k < 1500, |u| std::time::Instant::now() < u) {
        k += 1;
        let mut b = seeds[k % seeds.len()].clone();
        for _ in 0..1 + rng.below(8) {
            mutate(&mut rng, &mut b, words);
        }
        let mut input = vec![rng.next() as u8];
        input.extend(b);
        check::page_input(&input);
    }
    if secs.is_some() {
        eprintln!("MUTATED {k} pages read, the property held");
    }
}

/// HTML of the reader's own, each shape a piece reads (the fuzz target's seeds too).
fn seed_pages() -> Vec<Vec<u8>> {
    [
        "<!doctype html><html><head><title>t</title><base href=/x/></head><body><main><h1 id=a>A <a href=#a>§</a></h1>\
         <p>text <b>bold</b> <code>c`d</code></p><ul><li>one<ol start=3><li>two</li></ol></li></ul></main></body></html>",
        "<html><body><table><caption>c</caption><tr><th>a</th><th colspan=2>b</th></tr><tr><td><a href=y>y|z</a></td></tr></table>\
         <dl><dt>t</dt><dd>d</dd></dl><blockquote><p>q</p><pre><code class=language-rs>let a = 1;</code></pre></blockquote></body></html>",
        "<html><head><meta charset=euc-kr><meta http-equiv=refresh content=\"5; URL='next.html'\"></head><body>\
         <noscript>JS</noscript><script>x()</script><header>h</header><article><header>ah</header><p>p</p></article></body></html>",
    ]
    .iter()
    .map(|s| s.as_bytes().to_vec())
    .collect()
}

/// The fuzz target's seeds (`fuzz/parsers/seeds/web/`): the small fixtures and `seed_pages`, each read as it came
/// (the first byte 0: guessed, its type sniffed).
#[test]
#[ignore = "writes fuzz/parsers/seeds: by hand"]
fn writes_fuzz_seeds() {
    let fixtures = ["blog-redirect", "spa", "issue", "refresh"].map(|n| (n.to_string(), bytes(n)));
    let own = seed_pages().into_iter().enumerate().map(|(k, b)| (format!("own-{k}"), b));
    let seeds = fixtures.into_iter().chain(own).map(|(n, b)| (n, [&[0u8][..], &b].concat()));
    crate::mutate::fuzz_seeds("web", seeds);
}

// ── :web ──

fn feed(ed: &mut crate::editor::Editor, keys: &str) {
    use crate::key::{Code, Key};
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key = if c == '<' {
            let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
            name.parse().unwrap()
        } else {
            Key::plain(Code::Char(c))
        };
        ed.handle_key(key);
    }
}

fn settle_until(ed: &mut crate::editor::Editor, done: impl Fn(&crate::editor::Editor) -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !done(ed) {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let ev = ed.events.recv_timeout(left).expect("the page read before the timeout");
        ed.handle_event(ev);
    }
}

fn toasts(ed: &crate::editor::Editor) -> Vec<String> {
    ed.toasts.iter().map(|t| t.text.clone()).collect()
}

/// `:web <path>` from the project's folder, an ignored folder's as `:open` takes it: the page read on a worker into
/// a buffer of its own, named by its title, in markdown; how it read on the status line. A file that isn't a page
/// says what it is and opens nothing.
#[test]
fn web_reads_a_saved_page_into_a_buffer_named_by_its_title() {
    let root = crate::scratch::Scratch::new("web-saved");
    std::fs::create_dir_all(root.join("saved")).unwrap();
    std::fs::write(root.join(".gitignore"), "saved/\n").unwrap();
    std::fs::copy(dir().join("pages/issue.html"), root.join("saved/issue page.html")).unwrap();
    std::fs::write(root.join("saved/a.pdf"), b"%PDF-1.7\n").unwrap();
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.root = std::fs::canonicalize(root.path()).unwrap();
    let first = ed.doc().id;

    feed(&mut ed, ":web 'saved/issue page.html' https://github.com/ethnw/tarae/issues/12<ret>");
    // (its grammar loads on a worker after it)
    settle_until(&mut ed, |ed| ed.doc().id != first && ed.doc().lang_name().is_some());
    let doc = ed.doc();
    assert_eq!(
        doc.title.as_deref(),
        Some("web · github.com › web: read a saved page into a buffer · Issue #12 · ethnw/tarae · GitHub")
    );
    assert!(doc.text.to_string().contains("\n# web: read a saved page into a buffer #12\n"));
    assert_eq!(doc.lang_name(), Some("markdown"));
    assert!(
        toasts(&ed).iter().any(|t| t.starts_with("main · 3k chars · ") && t.ends_with(" · UTF-8")),
        "{:?}",
        toasts(&ed)
    );
    // Its links went to the address given
    assert!(doc.text.to_string().contains("[web](https://github.com/ethnw/tarae/issues?q="));

    // Again: the one buffer, read anew
    let docs = ed.docs.len();
    feed(&mut ed, ":web 'saved/issue page.html' https://github.com/ethnw/tarae/issues/12 page<ret>");
    settle_until(&mut ed, |ed| ed.toasts.iter().any(|t| t.text.starts_with("page · ")));
    assert_eq!(ed.docs.len(), docs);

    let showing = ed.doc().id;
    feed(&mut ed, ":web saved/a.pdf<ret>");
    settle_until(&mut ed, |ed| toasts(ed).iter().any(|t| t.starts_with("web: a PDF")));
    assert_eq!((ed.doc().id, ed.docs.len()), (showing, docs), "no buffer for what isn't a page");
}

/// `:web https://…` with the session's copy of that page (fetched within `KEEP`, by the address asked or the one it
/// came from) opens it with no network, said with its age; an address the fetch refuses is said at once, nothing
/// fetched; `:web-stop` with nothing under way says so.
#[test]
fn web_fetches_a_page_or_takes_the_session_s_copy() {
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    let f = super::fetch::Fetched {
        asked: "http://kept.test/a".into(),
        url: "https://kept.test/a".into(),
        status: 200,
        content_type: Some("text/html".into()),
        bytes: b"<title>Kept</title><main><h1>A kept page</h1><p>Its text.</p></main>".to_vec(),
        from: Vec::new(),
        hops: 0,
        upgraded: true,
        said: None,
        cut: false,
        took: std::time::Duration::from_millis(40),
        elsewhere: None,
    };
    let gone = ed.session.pages.put(std::sync::Arc::new(f), crate::clock::now());
    assert!(gone.is_empty());
    let first = ed.doc().id;
    feed(&mut ed, ":web https://kept.test/a<ret>");
    settle_until(&mut ed, |ed| ed.doc().id != first);
    assert_eq!(ed.doc().title.as_deref(), Some("web · kept.test › Kept"));
    assert!(ed.doc().text.to_string().contains("# A kept page"));
    let said = |t: &String| t.ends_with(" · UTF-8 · read just now · over https");
    assert!(toasts(&ed).iter().any(said), "{:?}", toasts(&ed));

    for (typed, said) in [
        (":web https://user:pw@kept.test/<ret>", "an address with a user or password in it isn't fetched"),
        (":web https://10.0.0.1/<ret>", "a bare address isn't fetched"),
        (":web-stop<ret>", "no page is being fetched"),
    ] {
        let before = toasts(&ed).len();
        feed(&mut ed, typed);
        assert!(toasts(&ed)[before..].iter().any(|t| t.starts_with(said)), "{typed}: {:?}", toasts(&ed));
        assert!(ed.session.web_fetch.is_none(), "{typed}: nothing under way");
    }
}

/// The session keeps only a whole 2xx page that isn't this machine's: the same address again opens the copy, but an
/// error's page, one maybe cut short and a `localhost` one (a dev server changes under you) are fetched each time;
/// `:web!` fetches again whatever was kept, the new copy kept in its place.
#[test]
fn web_keeps_only_whole_pages_and_web_bang_fetches_again() {
    use super::fetch::tests::{Reply, ok, path, reply, server, way, yours_rules};
    let (at, heads) = server(true, |head| match path(head) {
        "/missing" => reply(
            "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: 11\r\n\r\n<p>gone</p>",
        ),
        "/cut" => Reply {
            close_notify: false,
            ..reply("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<p>to the close</p>")
        },
        _ => ok("<title>A</title><main><h1>A page</h1><p>Its text.</p></main>"),
    });
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.web_way =
        Some(std::sync::Arc::new(move |stop| super::fetch::Way { stop, ..way(at, yours_rules()) }));
    // (how many requests the server had for what `typed` opened)
    let asked = |ed: &mut crate::editor::Editor, typed: &str| {
        let before = ed.doc().id;
        feed(ed, &format!("{typed}<ret>"));
        settle_until(ed, |ed| ed.doc().id != before && ed.session.web_fetch.is_none());
        heads.try_iter().count()
    };
    let kept =
        |ed: &crate::editor::Editor, url: &str| ed.session.pages.get(url, crate::clock::now()).is_some();
    let local = format!("https://localhost:{}/a", at.port());
    for (url, keeps) in [
        ("https://page.test/a", true),
        ("https://page.test/missing", false),
        ("https://page.test/cut", false),
        (local.as_str(), false),
    ] {
        assert_eq!(asked(&mut ed, &format!(":web {url}")), 1, "{url}: fetched");
        assert_eq!(kept(&ed, url), keeps, "{url}");
        assert_eq!(asked(&mut ed, &format!(":web {url}")), usize::from(!keeps), "{url}: again");
    }
    let before = ed.session.pages.get("https://page.test/a", crate::clock::now()).map(|(at, _)| at);
    assert_eq!(asked(&mut ed, ":web! https://page.test/a"), 1, ":web! fetches again");
    let after = ed.session.pages.get("https://page.test/a", crate::clock::now()).map(|(at, _)| at);
    assert!(after > before, "the new copy kept in its place");
}

/// `:web https://…` through its way (servers and a proxy of the test's own standing in for yours): the step said while
/// it runs and gone once it lands, the page in its buffer; `:web-stop` ends one under way — its late result dropped,
/// no buffer, no error; a second `:web` replaces the first — the first's late result opens nothing and never takes
/// the second's step —, the session's copy answering the second too: the first stopped all the same, its step gone.
#[test]
fn web_fetches_through_its_way_and_a_stop_or_a_newer_one_drops_the_older() {
    use super::fetch::tests::{Reply, ok, path, reply, server, way, yours_rules};
    use std::sync::{Arc, Mutex, mpsc};
    let (release, gate) = mpsc::channel::<()>();
    let gate = Arc::new(Mutex::new(gate));
    let (at, heads) = server(true, move |head| match path(head) {
        "/a" => ok("<title>A</title><main><h1>A page</h1><p>Its text.</p></main>"),
        "/gated" => {
            Reply { gate: Some(gate.clone()), ..ok("<title>Gated</title><main><h1>G</h1><p>t.</p></main>") }
        }
        _ => Reply { hold: true, ..reply("") },
    });
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.web_way = Some(Arc::new(move |stop| super::fetch::Way { stop, ..way(at, yours_rules()) }));
    let stepping = |ed: &crate::editor::Editor| ed.toasts.iter().any(|t| t.step.is_some());
    let dropped = |n: usize| move |ed: &crate::editor::Editor| ed.session.web_dropped == n;

    let first = ed.doc().id;
    feed(&mut ed, ":web https://page.test/a<ret>");
    assert!(toasts(&ed).iter().any(|t| t.starts_with("reading page.test…")), "{:?}", toasts(&ed));
    settle_until(&mut ed, |ed| ed.doc().id != first);
    assert_eq!(ed.doc().title.as_deref(), Some("web · page.test › A"));
    assert!(!stepping(&ed) && ed.session.web_fetch.is_none(), "the step over once it landed");
    assert_eq!(
        ed.session.pages.get("https://page.test/a", crate::clock::now()).map(|(_, f)| f.status),
        Some(200)
    );

    let (showing, docs) = (ed.doc().id, ed.docs.len());
    feed(&mut ed, ":web https://page.test/held<ret>");
    heads.recv_timeout(std::time::Duration::from_secs(5)).expect("the first request came");
    heads.recv_timeout(std::time::Duration::from_secs(5)).expect("the held one came");
    feed(&mut ed, ":web-stop<ret>");
    assert!(!stepping(&ed) && toasts(&ed).iter().any(|t| t == "web: stopped"), "{:?}", toasts(&ed));
    let said = toasts(&ed).len();
    settle_until(&mut ed, dropped(1));
    assert_eq!((ed.doc().id, ed.docs.len(), toasts(&ed).len()), (showing, docs, said), "nothing of it shown");

    feed(&mut ed, ":web https://page.test/held<ret>");
    heads.recv_timeout(std::time::Duration::from_secs(5)).expect("the older came");
    feed(&mut ed, ":web https://page.test/gated<ret>");
    heads.recv_timeout(std::time::Duration::from_secs(5)).expect("the newer came");
    settle_until(&mut ed, dropped(2));
    assert!(stepping(&ed), "the older's late result left the newer's step: {:?}", toasts(&ed));
    assert!(!toasts(&ed).iter().any(|t| t.starts_with("web: ") && t != "web: stopped"), "{:?}", toasts(&ed));
    assert_eq!((ed.doc().id, ed.docs.len()), (showing, docs));
    release.send(()).unwrap();
    settle_until(&mut ed, |ed| ed.doc().id != showing);
    assert_eq!(ed.doc().title.as_deref(), Some("web · page.test › Gated"));
    assert!(!stepping(&ed));

    // (the newer answered from the session's copy: the older stopped first all the same)
    feed(&mut ed, ":web https://page.test/held<ret>");
    heads.recv_timeout(std::time::Duration::from_secs(5)).expect("the older came");
    assert!(stepping(&ed));
    feed(&mut ed, ":web https://page.test/a<ret>");
    assert!(!stepping(&ed) && ed.session.web_fetch.is_none(), "the older stopped, its step gone");
    settle_until(&mut ed, |ed| ed.doc().title.as_deref() == Some("web · page.test › A"));
    settle_until(&mut ed, dropped(3));
    assert!(heads.try_recv().is_err(), "the copy answered: nothing fetched");
    assert!(!stepping(&ed));
}

/// A FIFO with nothing writing to it: refused at once and said — never a worker held inside open(2).
#[test]
fn a_fifo_is_refused_without_waiting_for_a_writer() {
    use std::os::unix::ffi::OsStrExt;
    let root = crate::scratch::Scratch::new("web-fifo");
    let fifo = root.join("page.html");
    let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: a NUL-terminated path in the test's own folder
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let want = format!("{}: not a file", fifo.display());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(super::open::bytes_of(&fifo));
    });
    let got =
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("refused without waiting for a writer");
    assert_eq!(got.unwrap_err(), want);
}

/// A saved page's own address keeps its name whole — `#`, `?`, `%` and spaces encoded — so its relative links
/// resolve against its folder and its fragments stay on it.
#[test]
fn a_saved_pages_address_keeps_its_name_and_its_links_their_folder() {
    let url = super::open::file_url(Path::new("/saved/a b?/notes#1.html"));
    assert_eq!(url, "file:///saved/a%20b%3F/notes%231.html");
    assert_eq!(super::open::file_url(Path::new("/s/100%é.html")), "file:///s/100%25%C3%A9.html");
    let html = r##"<p><a href="other.html">other</a> <a href="#x">here</a></p>"##;
    let p = reader::read(html.as_bytes(), &url, Some("text/html"), Prefer::Page);
    let urls: Vec<&str> = p.links.iter().map(|l| l.url.as_str()).collect();
    assert_eq!(urls, ["file:///saved/a%20b%3F/other.html", "file:///saved/a%20b%3F/notes%231.html#x"]);
}

/// A page's buffer is read-only — an edit put back and said, insert mode left — so reading it again replaces it
/// with nothing of yours in it.
#[test]
fn a_pages_buffer_is_read_only_and_read_again_in_place() {
    use crate::key::{Code, Key};
    let root = crate::scratch::Scratch::new("web-read-only");
    std::fs::copy(dir().join("pages/issue.html"), root.join("issue.html")).unwrap();
    let mut ed = crate::editor::Editor::new(crate::config::Config::default());
    ed.session.root = std::fs::canonicalize(root.path()).unwrap();
    let first = ed.doc().id;
    feed(&mut ed, ":web issue.html<ret>");
    settle_until(&mut ed, |ed| ed.doc().id != first);
    let text = ed.doc().text.to_string();
    let typed = |ed: &mut crate::editor::Editor, keys: &str| {
        for c in keys.chars() {
            ed.handle_event(crate::event::Event::Key(Key::plain(Code::Char(c))));
        }
    };
    typed(&mut ed, "jdd");
    assert_eq!(ed.doc().text.to_string(), text, "put back");
    assert!(
        toasts(&ed).iter().any(|t| t == "a web page is read-only — :web reads it again"),
        "{:?}",
        toasts(&ed)
    );
    typed(&mut ed, "ix");
    assert_eq!(ed.mode, crate::editor::Mode::Normal);
    assert_eq!(ed.doc().text.to_string(), text, "nothing typed");

    let (docs, before) = (ed.docs.len(), toasts(&ed).len());
    feed(&mut ed, ":web issue.html page<ret>");
    settle_until(&mut ed, |ed| toasts(ed)[before..].iter().any(|t| t.starts_with("page · ")));
    assert_eq!(ed.docs.len(), docs, "the one buffer, read anew");
    assert!(ed.doc().read_only.is_some() && ed.doc().text != text.as_str());
}

// ── Times ──

/// The reader in-process, the best of 3 a page (web-plan P0 measured it in the plugin's runtime): the five pages,
/// and a 4 MB one — ISO 639's tables over and over, as P0's 4.1 MB Wikipedia comparison page is. Numbers for
/// `cargo perf`.
#[test]
fn perf_web_read() {
    if crate::test_results::unoptimized("web::tests::perf_web_read") {
        return;
    }
    let table = String::from_utf8(bytes("table")).unwrap();
    let (start, end) = (table.find("<main").unwrap(), table.rfind("</main>").unwrap());
    let inner = &table[start..end];
    let big = format!("<html><body>{}</main></body></html>", inner.repeat((4 << 20) / inner.len() + 1));
    assert!(big.len() > 4 << 20);
    let mut pages: Vec<(&str, Vec<u8>, f64)> =
        [("blog", 15.0), ("docsrs", 25.0), ("issue", 5.0), ("mdn", 25.0), ("spa", 5.0)]
            .into_iter()
            .map(|(n, budget)| (n, bytes(n), budget))
            .collect();
    pages.push(("4mb", big.into_bytes(), 400.0));
    // ECMA-262 (7.6 MB, no `<main>`: the pick scores all of it) isn't a fixture, for its size: by hand, its saved
    // copy named by `TARAE_WEB_ECMA` (target/sb/web/more/big.html, from https://tc39.es/ecma262/)
    if let Some(at) = std::env::var_os("TARAE_WEB_ECMA") {
        pages.push(("ecma262", std::fs::read(at).unwrap(), 600.0));
    }
    for (name, b, budget) in pages {
        let mut best = f64::MAX;
        for _ in 0..3 {
            let t = std::time::Instant::now();
            let p = reader::read(&b, "https://example.com/", None, Prefer::Guess);
            best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            assert!(!p.text.is_empty());
        }
        let line = format!("PERF web_read_{name}_ms {best:.2} budget={budget} noise=1\n");
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        assert!(best < budget, "{name}: {best:.1} ms (budget {budget})");
    }
}
