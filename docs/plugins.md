# Plugins

Most of what tarae does is built in, and language features stay in the core. Plugins are for the rest: commands of
your own, written in Rust and compiled to WebAssembly. They run sandboxed on tarae's own small runtime. A plugin gets
the buffer and its selections, and answers with what tarae should do. Anything beyond the buffer — a program, a
file, the network — it has to ask for, and only gets once you approve.

- [Installing](#installing)
- [Writing one](#writing-one)
- [What a plugin sees](#what-a-plugin-sees)
- [What it can do](#what-it-can-do)
- [Events](#events)
- [Beyond the buffer](#beyond-the-buffer)
- [Its panel, its line, its buffers](#its-panel-its-line-its-buffers)
- [The SDK](#the-sdk)
- [Limits](#limits)
- [When tarae refuses a plugin](#when-tarae-refuses-a-plugin)

## Installing

A plugin is one `.wasm` file in `~/.config/tarae/plugins/` (or `$XDG_CONFIG_HOME/tarae/plugins/`). Put it there and a
running tarae loads it. Replace it and tarae reloads it; remove it and its commands go away.

```sh
tarae plugin list          # what's installed, and each plugin's commands (or why it doesn't load)
```

A plugin's commands work like tarae's own:
- **typed**: `:words`, `:title` (with arguments: `:name args`)
- **from the palette** (`space ?`): listed under the plugin's description for them
- **from keys**: bind `":title"` in `[keys.normal]`, as with any typed command

A command name tarae already uses, or one another plugin took first, is skipped with a warning.

With no plugins, nothing runs: no thread, no runtime loaded.

## Writing one

You need Rust and its WebAssembly target:

```sh
rustup target add wasm32-unknown-unknown
tarae plugin new my-plugin       # a crate from the template: :words and :title
cd my-plugin && tarae plugin dev # builds it on every save; your running tarae reloads it
```

A complete plugin (the template, with only its `:words`):

```rust
wit_bindgen::generate!({ path: "wit", world: "plugin" });

struct Plugin;

impl Guest for Plugin {
    fn commands() -> Vec<Command> {
        vec![Command { name: "words".into(), doc: "Count the words in the selections".into(), text: false }]
    }

    fn run(name: String, cx: Context) -> Vec<Action> {
        let n: usize = cx.selections.iter().map(|s| s.text.split_whitespace().count()).sum();
        vec![Action::Status(format!("{n} words"))]
    }

    // What it doesn't use, it answers with nothing
    fn subscriptions() -> Vec<Subscription> { vec![] }
    fn permissions() -> Vec<Permission> { vec![] }
    fn on(_: Event, _: Context) -> Vec<Action> { vec![] }
    fn on_events(_: Vec<HostEvent>, _: Now) -> Vec<Action> { vec![] }
}

export!(Plugin);
```

- **The API** is [`wit/tarae.wit`](../plugin-sdk/wit/tarae.wit) in your crate. `wit-bindgen` turns it into the Rust
  types above.
- **Rust is what's supported**, with the `wit-bindgen` version the template pins. Other languages that produce the
  same kind of module may work, but only Rust is tested.
- `commands()`, `subscriptions()` and `permissions()` are called when the plugin loads (and again if it's restarted
  after a failure). `run()` is called for each command, `on()` for each [event](#events) it subscribed to, and
  `on-events()` with what came back from what it asked for ([beyond the buffer](#beyond-the-buffer)).
- A plugin keeps its state between calls, until it fails or is reloaded.

## What a plugin sees

`run` gets the command's name and a `context`:

| | |
|---|---|
| `args` | what followed the command: `:grab foo` → `"foo"` |
| `buffer` | `path`, `name`, `language`, `version`, `modified`, and `text` — the whole buffer, but only for commands registered with `text: true` |
| `selections` | each with `anchor`, `head`, `from`, `to`, its `text`, and the `line` its head is on |
| `primary` | which selection is the primary one |
| `now` | the time (`millis` since 1970) and 32 random bytes — a plugin has neither of its own; hand them to the [SDK](#the-sdk) |

Positions are byte offsets into the buffer.

## What it can do

`run` answers with a list of actions, which tarae carries out in order:

| Action | |
|---|---|
| `status`, `success`, `warning`, `error` | a message (errors are shown with the plugin's name) |
| `replace-selections` | one new text per selection |
| `edits` | `(from, to, insert)` changes anywhere in the buffer |
| `select` | new selections, as `(anchor, head)` in the buffer after the answer's edits; the first is the primary one |
| `run` | a tarae movement command (`move_next_word_start`, `goto_file_start`): only commands that move the cursor or selections run |
| `open` | a file |
| `status-bar` | a short text in the status line, for this buffer while it's in front, until replaced; `""` clears it |

An answer's edits (`replace-selections`, `edits`, `select`) are **one undo step**, applied where the first of them is
in the list, to the buffer and selections the command was given. The buffer in front stays in front. They're refused
if the buffer changed while the plugin ran or you're typing in it, if they overlap, or if a position (a `select`'s
too) isn't on a character boundary.

## Events

A plugin that answers `subscriptions()` with some of these gets `on(event, context)` when they happen. The context is
about the buffer it happened to, and the answer is carried out like a command's.

| Event | When |
|---|---|
| `opened` | a file is loaded (a big one: once its text is in) |
| `saved` | a buffer is written |
| `closed` | a buffer is about to close (there's nothing left to edit, but messages show) |
| `changed` | a buffer was edited and then left alone for 300 ms — not on every key |
| `before-save` | `:w` (and `:wq`, `:wa`) is about to write: the answer's edits go into the file. Saving waits for it, up to 2 seconds, then saves as is. Auto-save doesn't wait for plugins |
| `shown` | The buffer is now the one in front — switched to, its window focused, or opened — for a view of "this file". Once per change of buffer, not again while it stays |

```rust
fn subscriptions() -> Vec<Subscription> {
    // `text`: as for commands · `languages`: only those buffers (none: every buffer)
    vec![Subscription { event: Event::Changed, text: true, languages: vec![] }]
}

fn on(event: Event, cx: Context) -> Vec<Action> {
    let words = cx.buffer.text.unwrap_or_default().split_whitespace().count();
    vec![Action::StatusBar(format!("{words} words"))]
}
```

- **Nothing waits on a plugin while you type.** An event only puts work on the plugin thread. With no subscriber
  it's not even that.
- **One after another**: the plugins subscribed to an event are told in turn, each seeing the buffer as the one before
  left it.
- **No loops**: `changed` isn't sent for an edit a plugin made itself, only for yours.

## Beyond the buffer

A plugin that needs more says so in `permissions()`. When it loads, a card lists what it asks for, in words (and how
far it reaches: a program runs with any arguments, as you); until you press `y` it can do nothing beyond the buffer
(its buffer-only commands still work). The card takes `y` only once it has been in sight for half a second, so a key
meant for the code can't approve it. What you approve is kept, by the plugin's name; if a new version asks for
something else, the card asks again. Said no? The plugin can't bring the card back — `:plugin-approve <plugin>`
does.

```rust
fn permissions() -> Vec<Permission> {
    vec![
        Permission::Env("KUBECONFIG".into()),
        Permission::Read("~/.kube/**".into()), // `*` a name, `**` any folders
        Permission::Network,
    ]
}
```

| Permission | Lets it | Asked about besides |
|---|---|---|
| `run(program)` | `spawn` that program (a plain name, found on `PATH`): argv, no shell, in your home folder, in a process group of its own, with a plain environment — `PATH`, `HOME`, `USER`, the locale, `TERM`, `TMPDIR`, and the variables it may read (which it may also set for it: `AWS_PROFILE`) | — |
| `read(pattern)` | `read-file` what the pattern covers (up to 16 MB) | a file outside the patterns, once — `..` and symlinks don't lead out |
| `env(name)` | `read-env` that variable | — |
| `network` | `connect` over TCP | each new host:port, once |
| `project` | `read-file` in the project tarae started in, and `find` there (`"**/kustomization.yaml"`: absolute paths back, as the file picker lists them) | — |

Requests are actions, and what they bring comes back later to `on-events(events, now)`, in the order it happened:

| Action | Comes back as |
|---|---|
| `spawn` | `lines` as the program prints, then `exit` (its code, and its last lines on stderr) |
| `connect`, then `send` | `connected`, then `data` as it comes (up to 64 KB at a time), `closed` at the end |
| `read-file`, `read-env` | `file` (its bytes), `env` (its value, or none) |
| `find` (id, pattern) | `found` (the project's files it matches) |
| `write-own` (id, name, bytes) | `written` (where): a file in the plugin's own folder (in tarae's state folder — nowhere else; no permission, up to 1 MB), readable by you alone |
| `offer-install` (id, tool) | `installed` once you've installed it from the card (a tool a language pack knows how to install; once until tarae quits), or `failed` |
| `timer` (id, ms) | `tick` |
| `stop` (id) | nothing more: the program is stopped, the connection closed, the timer dropped |

Anything refused or failing comes back as `failed`, with why. Each request has an id of yours; one still in use is
replaced. A plugin has at most 16 programs, 16 connections and 256 timers at once, and 16 MB waiting to be sent on
a connection. When the plugin is dropped, reloaded or its permissions revoked, all it started ends (a program that
doesn't stop is killed 2 s later), and nothing meant for it before a reload reaches it after.

What comes back while the plugin is busy waits and goes to it in one call; while 4 MB of output waits, its
programs and connections aren't read — they wait for it, rather than tarae's memory growing.

`:plugin-permissions` lists each plugin's; `:plugin-revoke <plugin>` takes them back; `:plugin-approve <plugin>`
shows the card again.

## Beside the code, its tab, its panel, its line, its buffers

| Action | |
|---|---|
| `hints` (path, version, hints) | text at the end of a buffer's lines (a status beside the code), each in a tone — fine, of note, warning, error. For the buffer's text at that `version` only: edited, they wait until the plugin sends them again (it hears `changed`, and `saved` — a formatter may change it). Where a diagnostic or a note speaks, they give way |
| `side` | its tab in the side panel, beside files, outline and git: a tree of rows (depth, text, a dim detail, a tone, and where in a file it's defined), and notes — a line under a row, in its tone's color (why it isn't fine), not a row to land on. tarae folds it (`h` `l`), marks the row for the code under the cursor, and `enter` goes to a row's place — a row with no place, and keys the panel doesn't take, come to the plugin as `key` with the row's id. `open` shows the panel at the tab (without the keys), `focus` with them |
| `badge` | a short text in the status line, on every buffer (what it's connected to), in a tone |
| `pick` (id, title, items) | tarae's small picker with its items; the choice comes back as `answer` (its id, or none: `esc`). Like `prompt`, only in answer to you |
| `open` | a file — at a line, as `path:line` |

| Action | |
|---|---|
| `panel` | a table below the code — columns, rows (with an id each, if you like: `"namespace/name"`), a status line, and the keys it offers (`("d", "describe")`) for the header. A command's answer that shows it gives it the keys. With ids, the selection stays on its row as rows come, go and move |
| `prompt` (id, label) | the command line, labelled with the plugin's name and the label; the answer comes back (`answer`: the text, or none for `esc`). Only in answer to you — a command, a key in its panel, a line you typed — and not while you're typing something else (then `failed`) |
| `show` (a view) | a buffer of its own, in front, colored by its language — shown again with the same id, replaced |
| `append` (id, text) | added to that buffer's end; a cursor at the end follows it (a log). When you close it, the plugin is told (`failed`, with its id: time to stop following), and appending to it fails |

While the panel has the keys, tarae moves through its rows (`j` `k` `g` `G`, `C-d` `C-u`, the arrows, the wheel);
`esc` gives them back to the code, and `space` and `:` stay the editor's. Every other key comes to the plugin as
`key`, with the row selected and its id — the plugin decides what `d` or `ret` does. A click on the panel gives it
the keys. While the debugger or the terminal has its place, it's out of sight and takes no keys.

## The SDK

[`plugin-sdk/sdk`](../plugin-sdk/sdk) (`tarae-plugin`) goes beside the bindings:

- `now::set(millis, &entropy)` — hand it each call's `now` first thing. Then `getrandom` works (a ChaCha20 generator
  stirred with each call's bytes), and so do the crates that use it.
- `http` — requests written out (what's given can't add a line of its own); responses read as their bytes come
  (by length, chunked, until close; `100 Continue` skipped), and a streamed body cut into lines.
- `tls` (feature) — a TLS client over bytes: rustls with RustCrypto, no sockets. What it wants sent goes out with
  `send`, what comes back in `data` goes in; a client certificate from PEM if you have one.

[`plugin-sdk/kubernetes`](../plugin-sdk/kubernetes) uses all of it: the cluster in the code. Open a manifest and
each object in it has its state beside its `kind:` line (`2/3 ready · 1 ImagePullBackOff`, `Ready`, `not in
prod-eks`), live; the status line says which cluster (`⎈` the kubeconfig's current context); the side panel's
`k8s` tab is the file in front's, as the outline is (`shown`): its objects by kind, names under each — briefly: a number when it's fine (`3/3`,
`3 pods`, `↻2` if its pods restarted), and when it isn't, why on a line of its own, under the pods that say it
(newest first, `2m` if new). Below them, the other open files that aren't fine, to go to (`enter`); a file with
none says so. `C-w e` takes the keys to the tab and back. `enter` goes to where one is defined (a pod: its logs), `d` describes a pod (its
containers, how they last ended, conditions, events), `e` its events (a workload's with its pods'), `y` its live
YAML (a Secret's values masked), `a` all of that in a pick. `:kubernetes-here` does the same for the object under the
cursor. `:kubernetes` turns it on (the tab opens the first time) and then takes you to the tab;
`:kubernetes-off` turns it off. Any kind works — a kind's name for the API comes from the cluster, and one it
doesn't know says so. Whether an object is fine is kstatus's reckoning (what Flux and kpt wait on): deleted,
its latest generation not yet observed, `Reconciling`, `Stalled`, `Ready`, a rollout past its deadline — so any
CRD's objects read the same way. Browsing a cluster is kubectl's.

The kubeconfig is read in the plugin, TLS and the API run in WebAssembly, requests come from kube-core and types
from k8s-openapi; only its bytes go through tarae. It watches one kind in one namespace at a time (the ones the
open manifests hold) — only the object named when there's one of the kind there, and the pods its workloads
select — so what it holds and works on follows what's open, not the cluster's size: a list page by page, then
a watch; a watch that ends goes on, one whose version is too old
(410) lists afresh, and failures are tried again after a growing pause. Secrets are watched without their values.
Its words are kubectl's (a pod's status and restarts, ages). Where the cluster's object says otherwise than a
line, its value goes beside the line (`replicas: 1  → 2`): only lines read for sure (block YAML; not a block
scalar's text), fields the cluster has, compared by what they mean (`0.5` CPU is `500m`), never a Secret's — and who set it, from the object's
`managedFields` (server-side apply's record of each field's manager): `replicas: 1  → 3 · kubectl`. Why a
workload's pods aren't fine goes on the line that's why, from the pods' own status (nothing more is asked of the
cluster): an image that can't be pulled on its `image:` line (`ImagePullBackOff: not found`), a missing ConfigMap
or Secret on the line that names it, `OOMKilled ↻3` on `limits.memory`, `CrashLoopBackOff · exit 1` on
`command:`, a container running unready for over a minute on `readinessProbe:`, and each of the scheduler's
reasons on the request, `nodeSelector`/`affinity`, `tolerations` or `volumes` it's about. And what a line names
that isn't there, before a pod finds out: a ConfigMap (or the key it reads from it), a Secret, a ServiceAccount, a
claim, a pull secret, an Ingress's Service, an HPA's target — `no Secret app-token in prod`, `no key mode in
app-settings`, or `isn't applied yet` when an open manifest has it; a Service whose selector no pod has. Only the
names it needs are watched; a reference marked `optional`, or that a template writes, is left alone, and one a
Kustomize build renamed (a prefix, a generator's hash) is looked for by the name the build gave it. A line says all
that's said of it in one hint. From the
kubeconfig: certificates and keys (inline or as files, relative to it), tokens (inline or `tokenFile`), a server
URL with a path or an IPv6 address, `tls-server-name`, and EKS's exec credentials: it runs `aws eks get-token`
(with the kubeconfig's `AWS_PROFILE` and the like) for a token, and again a minute before it expires or on a 401.

Kustomize: a file a kustomization takes in is read through a build — the project's kustomizations are found
(`project` permission), each root (one no other takes in) that ends up with the file is built by `kustomize build`
through a kustomization of the plugin's own asking for each object's origin, and a document is found in the
cluster as the build made it: `replicas: 1` in a base says `2/2 ready (via prod)`, under prod's name prefix and
namespace. Of several roots, the one the cluster has the most of. A kustomization's `configMapGenerator` and
`secretGenerator` names say how the ConfigMap they made is doing. Saving a kustomization, or a file one takes in,
builds again. No kustomize: it says so, and offers to install it.

Helm: a chart's template is read through `helm template` for each release of the chart the cluster has. The
project's charts are its `Chart.yaml`s (a subchart's templates are its parent's); a release is found by the labels
Helm's own charts put on what they install — `helm.sh/chart: <chart>-<version>` and `app.kubernetes.io/instance:
<release>` — on the cluster's deployments, stateful sets, daemon sets, services, config maps and service accounts
(cluster-wide, or the context's namespace if that isn't allowed), so it's the same whether the helm CLI, Flux or
Argo CD installed it. Each object the render puts out says its template (`# Source:`), so a template's k-th
document of a kind is the render's k-th object of that kind from it: `kind: Deployment  1/1 ready (via shop-helm)`.
A line a template writes shows what it made (`replicas: {{ .Values.replicaCount }}  = 2`; in a `range`, every
item's); a line written out is compared as in any manifest. Each release renders with the values it was given
(`helm get values` — helm reads them from the release's Secret, with your kubeconfig; the plugin itself reads no
Secret's values): written to a file of the plugin's own for `helm template --values`, which is emptied once it's
rendered. Without read access to them it renders with the chart's own, and says so where a document isn't
rendered. A document its values leave out says so, and a hook says it's one. A
chart's `values.yaml` says which releases of it there are, and beside a value a template writes as it is
(`replicas: {{ .Values.replicaCount }}`, quoted or piped through `quote`, `int` and the like) what each release
has there where it isn't the value, and who set it: `→ 2 · release values` when the release was given it,
`replicaCount: 1  → 3 · kubectl` when someone changed it since. A value written with others
(`"{{ .Values.image.repository }}:{{ .Values.image.tag }}"`), defaulted or worked out isn't said — whose it is
can't be told. The chart's templates are found and read for it, open or not. Saving a chart's file renders again. No helm: it says
so, and offers to install it.

Nothing is Flux's or Argo CD's own: their objects read like any CRD's (kstatus), and what they install is found by
Kubernetes' and Helm's own marks. Not yet: other credential programs (gcloud, kubelogin), `proxy-url`, a
release's own values; never `insecure-skip-tls-verify`.

## Limits

- **No imports**: no WASI, no host functions. A plugin reads what tarae gives it and answers; what it asks beyond
  the buffer comes back as events. Build for `wasm32-unknown-unknown` (the template does).
- **Time**: about a second of work per call, then it's stopped. Plugins run on their own thread, one call at a time,
  so a slow one never holds up typing, only other plugin commands. (Applying a very large answer, like hundreds of
  thousands of edits, does take the editor a moment.)
- **Memory**: 256 MB per plugin.
- **Answers**: up to 64 MB of text and lists. Messages are shown without control characters (so no terminal escape
  sequences) and up to 2000 characters. Up to 256 commands per plugin.
- **Commands it can run**: movement only. A plugin can't reach `:sh`, `:w`, or anything else with effects outside
  the buffer.
- **WebAssembly features**: what rustc turns on by default for `wasm32-unknown-unknown`. No SIMD (`+simd128`),
  threads, or exceptions.

## When tarae refuses a plugin

`tarae plugin list` and the editor say why:

| Message | Fix |
|---|---|
| “imports something — plugins get no imports” | build for `wasm32-unknown-unknown`, not `wasip1`/`wasip2`, and don't use crates that need the OS |
| “uses SIMD” | build without `-C target-feature=+simd128` |
| “built for tarae plugin API 0.N, this tarae speaks 0.M” | rebuild against this tarae's `wit/tarae.wit` (`tarae plugin new` has it). Until 1.0 every 0.x breaks plugins |
| “has no `…` export” / “doesn't match tarae's plugin API” | build it from the template, with the `wit-bindgen` version it pins |
| “ran too long and was stopped” | a loop that never ends, or too much work for one call |
| “plugins can run movement commands only” | `run` named something that isn't a movement command |
| “its answer is too big” | over 64 MB of text and lists in one answer |
| “crashed (a panic?)” | the plugin panicked |
| `failed`: “it didn't ask to …” | add it to `permissions()` (then you approve it again) |
| `failed`: “its permissions wait for your approval” | `y` on its card, or `:plugin-permissions` |
