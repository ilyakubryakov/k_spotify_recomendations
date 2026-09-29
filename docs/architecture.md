# Architecture notes

Rust CLI/TUI that curates Spotify playlists with an LLM. Read this before
changing anything; it records the decisions that aren't obvious from the code.

## Layout

```
src/
  domain/     vocabulary types. No I/O, no HTTP, no SQL.
  config/     layered TOML + env. presets.rs holds the built-in briefs.
  spotify/    OAuth (PKCE) + rate-limited Web API client.
  llm/        provider-agnostic chain: anthropic | openai | ollama | gemini.
  storage/    SQLite: library mirror, play history, feedback, snapshots.
  engine/     orchestration. analyze → prompt → resolve → filter → publish.
  tui/        app.rs is state + input, ui.rs is rendering. Strictly separated.
  i18n/       one struct of &'static str per language.
```

Dependencies point downward only. `engine` knows `spotify`, `llm`, `storage`;
none of them knows `engine`. Nothing below `commands`/`tui` knows a terminal
exists — that's what makes the pipeline testable and the TUI optional.

## Build and test

```bash
cargo test                  # no network needed; HTTP is mocked
cargo clippy --all-targets  # unwrap/panic are denied in non-test code
```

`tests/support/mod.rs` is a hand-rolled scripted HTTP server. It exists rather
than a mocking crate because these tests assert on exact request bytes and
control exact response bytes (SSE framing, `Retry-After`, truncated pages).
`MockServer::start_with` hands you the base URL before you build the replies —
needed whenever a response body has to embed the server's own address, like
Spotify's `next` pagination link.

## Things that will bite you

**Model API drift.** `temperature`, `top_p`, `top_k` and
`thinking.budget_tokens` are *rejected with a 400* on the current Anthropic
models. Structured output is `output_config.format`, not `output_format`.
`stop_reason` can be `"refusal"` on an HTTP 200 — always check it before
reading `content`. `tests/llm_wire.rs` pins all of this; if you change the
request shape, that file should fail.

**Gemini's schema dialect** is an OpenAPI subset that rejects
`additionalProperties`, which every other provider requires. `schema::for_gemini`
translates. Don't "simplify" it away.

**Spotify rate limits** are a rolling window and `Retry-After` can be minutes.
Everything goes through `util::retry::with_retry` so the two clients can't
disagree about what's retryable. Undershooting a `Retry-After` is how you get
soft-banned.

**Play history is irreplaceable.** Spotify returns only the last 50 plays ever.
The local `plays` table is the only long-run frequency signal that exists, which
is why `cache clear` demands `--yes` and why retention defaults are generous.

**Skips are inferred, not observed.** There is no skip event in the Web API.
`engine::feedback::classify_play` derives it from the gap to the next play. It
over-reports (a pause looks like a skip), so the weight is deliberately smaller
than an explicit thumbs-down. Don't raise it without evidence.

**Snapshots before destructive writes.** `engine::publish` refuses to overwrite
a playlist if the snapshot fails. That's intentional: it's the one case where
carrying on loses data permanently.

**`Secret` serialises as `"<redacted>"`.** That's why the token store has its own
`StoredTokens` struct with plain fields and an explicit `.expose()`, and why
preferences live in `preferences.json` rather than being written back into the
commented `config.toml`.

**SIGPIPE.** Rust ignores it, which makes `println!` panic when piped into
`head`. `main::restore_default_sigpipe` puts it back. Don't remove it.

## Conventions

- No `unwrap`/`expect`/`panic!` outside tests — denied by clippy in Cargo.toml.
- Errors carry retryability as data (`AgentError::is_retryable`), not as a guess
  at the call site.
- Exit codes follow `sysexits.h` so cron can distinguish "retry" from "fix me".
- Comments explain *why*, not *what*. If a line needs a comment to say what it
  does, rewrite the line.
- Tests are named as the property they prove, not `test_foo`.
- Every user-visible string in the TUI goes through `i18n::Strings`. Adding a
  field there is a compile error in all four languages until each supplies it —
  that's the point.

## Verified platforms

| Platform        | Build + tests | Scheduler                                   |
|-----------------|---------------|---------------------------------------------|
| Linux x86_64    | yes           | systemd --user timer installed, verified, removed |
| macOS arm64     | yes           | launchd LaunchAgent installed (`plutil -lint` clean), verified, removed |
| Windows 11 x64  | yes (MSVC)    | `schtasks` task registered, queried, removed |

All three were exercised on real hardware, including the installers. Windows
testing runs through a local libvirt VM with VS Build Tools 2022 and rustup
installed under the user profile.

Four bugs only Windows could find, all now covered by tests:

- `install.ps1` must be **pure ASCII**. PowerShell 5.1 decodes a BOM-less `.ps1`
  with the system ANSI codepage, so one em-dash took the whole parser down with
  an error pointing at an unrelated line.
- `[ValidateSet]` on a `param()` entry breaks `irm ... | iex`: piped through
  `iex` the param block is evaluated in the caller's scope and the validator
  rejects its own empty default.
- `Die` must `throw`, not `exit` — under `iex` an `exit` closes the user's shell
  over something as ordinary as a 404.
- `current_exe().canonicalize()` returns a `\\?\` extended-length path on
  Windows, which then gets baked into the scheduled task.

The release binary links only libc, libm and libgcc — SQLite is compiled in and
TLS is rustls — so there is nothing to install alongside it.

## CI / release

`.github/workflows/ci.yml` runs fmt, clippy, the suite on all three platforms,
an MSRV check against `rust-version` in Cargo.toml, installer linting and
`cargo audit`. `release.yml` fires on a `v*` tag, builds six targets, and
publishes archives plus a `SHA256SUMS` the installers verify against.

Asset names are load-bearing: `spotify-agent-<target>.tar.gz` / `.zip`, with no
version in the filename, so `releases/latest/download/` resolves. Changing that
scheme means changing both installers.

The tag must match `version` in Cargo.toml — the release job checks and fails
otherwise.

## Where to start

- Changing what the model is asked: `llm/prompt.rs`. The briefs in
  `config/presets.rs` do more work than anything else in the repo.
- Changing what gets filtered out: `engine/filter.rs` and `engine::select`.
- Changing what "least engaging" means for rolling playlists:
  `engine/rolling.rs::Incumbent::engagement`.
