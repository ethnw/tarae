# tarae plugin SDK

- [`wit/tarae.wit`](wit/tarae.wit): the plugin API — what a plugin sees and what it can answer
- [`template/`](template): what `tarae plugin new` writes (plus the WIT and a `.cargo/config.toml` for
  `wasm32-unknown-unknown`)
- [`demo/`](demo): a plugin with a command for every action, and some that misbehave on purpose. Its build,
  [`demo.wasm`](demo.wasm), is what tarae's tests load.
- [`sdk/`](sdk): `tarae-plugin`, the Rust helpers — the time and randomness tarae hands over, HTTP read as it
  comes, TLS over a plugin's connections
- [`kubernetes/`](kubernetes): the cluster in the code — each object's state beside its manifest, a side tab, in pure Rust (kube-core, k8s-openapi, rustls)

How to write and install a plugin: [docs/plugins.md](../docs/plugins.md).

## License

What a plugin is built from — [`wit/`](wit), [`template/`](template) and [`sdk/`](sdk) — is under
[MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your choice, so a plugin can be under any license, closed ones
too. tarae itself stays GPL-3.0-or-later.
