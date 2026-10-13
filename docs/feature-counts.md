# Feature counts

Which of tarae's features you use, and which you never do — counted on this machine only, to steer what gets
polished, checked or dropped. Off until you turn it on in your own config:

```toml
[features]
count = true
```

(or `:set features.count true` until you quit). A project's `.tarae.toml` can't turn it on. Off, nothing is
counted and nothing is written — there's no file.

## What's counted

Each with how often and the last day it was used:

- **commands** — the command a key ran (`move_line_down`), never the key; chosen in the palette, counted apart;
- **typed commands** — a `:` command by its first name (`:w` and `:write` are one), never what follows it; a line
  naming no command tarae has isn't counted at all;
- **plugin commands** — by the name the plugin registered;
- **cards** — each kind asked (`run`, `read`, `change-set`…) and its answers by letter (`y 20 · a 5 · n 5`);
- **side panel tabs** shown, **pickers** opened (by kind — `files`, `buffers` — never their title);
- **Claude's tools** — every call in the chat, by tool; a name not in tarae's list is `other`.

Nothing of content: no paths, file names, arguments, text, queries or model output. Clicks aren't counted.

## A person or a driver

Keys are one count. A person typing and a driver sending keys (zellij's `write-chars`, a script) reach tarae
the same way — as the terminal's input — so tarae can't tell them apart, and doesn't guess. Claude's tool calls
come over its own stream: counted apart, exactly. (Keys sent over the UI socket, once that exists, would be known
to be a driver's and get a count of their own.)

## Seeing them

- `:features` — a buffer: each area's used features, most used first, then **Never used**, listed from the command
  table, the keymap (with the key bound to each), the card kinds, panels, pickers and tools — so a 0 is real.
- `tarae state --features` (`--json`) — the same, every feature with its count (`src/snapshots/features.json`
  pins the shape).
- `:features-reset` — empties them, after a card. A tarae running beside this one adds what it counted since it
  last saved.

## Cost and where they're kept

- A key's count is an index and an add (`perf_budget_feature_count`: 1.3 ns); a batch of events reads the clock
  once for today (24 ns).
- Nothing wakes tarae for it: what's counted goes to `$XDG_STATE_HOME/tarae/features.json` (0600) on a worker
  with the first batch of events 5 minutes after the last save, and when tarae ends — a crash loses at most those
  minutes. One file per user: taraes running side by side each add theirs under a lock, none overwrites another's.
- Never sent anywhere: the counter has no network code (`the_counter_has_no_way_out`), Claude's level of the UI
  state doesn't hold them, and the UI socket refuses Claude's processes.

## How other editors do it (from memory — not yet checked against their sources)

- **JetBrains** — the Productivity Guide: per feature, times used and last used, kept locally, features never used
  listed too. Separate from its opt-in usage statistics, which are sent. *Taken:* count + last used, zeros listed.
- **VS Code** — `telemetry.telemetryLevel` (all / error / crash / off) for what's sent to Microsoft; what's
  recorded can be shown locally. *Taken:* you can see exactly what's kept. *Not taken:* sending, opt-out.
- **Zed** — telemetry settings for metrics and diagnostics, sent, on by default, a log to read them. As VS Code.
- **Helix, Neovim** — no telemetry. *Taken:* no network at all.
- **Emacs's `keyfreq`** (a package) — counts commands, not keys, in a local file. *Taken:* the command, not the key.
