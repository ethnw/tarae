title: web: read a saved page into a buffer · Issue #12 · ethnw/tarae · GitHub
read: main
charset: UTF-8
links: 22
anchors: 1
---
**[tarae](https://github.com/ethnw/tarae)**

# web: read a saved page into a buffer #12

Open

### Labels

[web](https://github.com/ethnw/tarae/issues?q=state%3Aopen%20label%3A%22web%22) [reader](https://github.com/ethnw/tarae/issues?q=state%3Aopen%20label%3A%22reader%22)

## Description

[[@eth219]](https://github.com/eth219) [eth219](https://github.com/eth219) opened [on Oct 23, 2026](#issue-3001)

The web reader turns a saved page into text tarae can show: its `<main>` when it has one, every link where it stands, each code block fenced with its language.

What it should leave out:

- what the page runs, styles or draws (`script`, `style`, inline `svg`)
- the site's chrome: its header, its navigation, its footer
- comments the site hid

```rust
let page = web::read(&bytes, url, None, Prefer::Guess);
assert_eq!(page.read, Read::Main);
```

## Activity

1. ### tarae-reviewer commented on Oct 23, 2026

   [[@tarae-reviewer]](https://github.com/tarae-reviewer) [tarae-reviewer](https://github.com/tarae-reviewer) [on Oct 23, 2026](#issuecomment-3002)

   A page's bytes are anyone's: the reader must never panic on them, whatever they hold, and say when a limit cut a page short.

2. ### eth219 commented on Oct 23, 2026

   [[@eth219]](https://github.com/eth219) [eth219](https://github.com/eth219) [on Oct 23, 2026](#issuecomment-3003)

   Agreed. The reader goes into the core, so Claude's `read` always has it.

   > never panic on them, whatever they hold

   Fuzzed beside the other readers of what anyone can send.

3. [eth219](https://github.com/eth219) added a commit that references this issue [on Oct 23, 2026](#event-3004) [web: the reader in the core](https://github.com/ethnw/tarae/commit/0123456789abcdef0123456789abcdef01234567) [0123456](https://github.com/ethnw/tarae/commit/0123456789abcdef0123456789abcdef01234567)
4. ### spam-account commented on Oct 23, 2026

   [[@spam-account]](https://github.com/spam-account) [spam-account](https://github.com/spam-account) [on Oct 23, 2026](#issuecomment-3005) · Hidden as off-topic
5. ### other-account commented on Oct 23, 2026

   [[@other-account]](https://github.com/other-account) [other-account](https://github.com/other-account) [on Oct 23, 2026](#issuecomment-3006) · Hidden as off-topic
