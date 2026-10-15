# Mirage

Mirage renders application configuration from shared TOML data, combining Figment profiles with
MiniJinja templates and Luau extensions. Each generated file is written beside its template, where
the daemon keeps it up to date as inputs change or runtime updates arrive through `miragectl`.

Before replacing any destination files, Mirage renders and stages every output in the pass. Each
replacement is atomic, although applications can observe different files changing at different
times as the pass commits its results.

Mirage is under active development and is intended for Linux desktop configurations.

## Requirements

- stable Rust and Cargo
- a C++17 toolchain for the bundled Luau runtime
- filesystem notification support and Unix domain sockets
- read access to configure files and templates
- write access to the template directories and control socket location

## Setup

Build both binaries from the checkout with Cargo.

```sh
cargo build --release --workspace
```

This produces `target/release/mirage` and `target/release/miragectl`, which you can run directly.
To install them into Cargo's binary directory, use the following commands.

```sh
cargo install --path mirage
cargo install --path miragectl
```

## Example

Create `~/.config/mirage` for the manifest and configure data, along with `~/.config/example` for
templates. In `~/.config/mirage/.mirage.toml`, select the configure file and template directory.

```toml
[configure]
profile = "default"
template = "~/.config/example"

[[candidate]]
path = "theme.toml"
policy = "override"
```

The manifest points to `~/.config/mirage/theme.toml`, where the `default` and `powersave` profiles
define the values that templates will use.

```toml
[default.theme]
background = "#202020"
foreground = "#eeeeee"

[powersave.theme]
background = "#000000"
```

With those values in place, create `~/.config/example/terminal.toml.jinja` to use them in an
application configuration.

```jinja
[colors.primary]
background = "{{ theme.background }}"
foreground = "{{ theme.foreground }}"
```

Run Mirage once to render the templates, then inspect the generated file.

```sh
mirage --oneshot -C ~/.config/mirage
cat ~/.config/example/terminal.toml
```

For continuous updates, start the daemon, which renders the initial context before waiting for
changes.

```sh
mirage -C ~/.config/mirage
```

While the daemon is running, select the `powersave` profile from another terminal to render with
its values.

```sh
miragectl profile powersave
```

## How it works

The configure subsystem builds the render context by expanding candidate paths, combining their
TOML data, and selecting the active Figment profile. Once resolved, the context is published to the
hydration worker, which uses it to render the templates.

For each pass, the worker discovers templates recursively and builds a MiniJinja environment with
the configured Luau module's exports alongside the built-in functions and filters, including
`tojson`. Rendering runs on a dedicated thread, where a Tokio runtime drives asynchronous Luau
calls to completion.

Discovery includes dot directories and uses `.jinja` as the default template extension. Mirage
removes that extension to name the output, so `terminal.toml.jinja` produces `terminal.toml` beside
the source with the same permissions. Every discovered template is rendered on each pass, including
templates for large assets such as lookup tables.

When relevant inputs change, the worker waits for the configured quiet interval before starting
another pass. Further relevant events restart that interval, allowing a burst of changes to settle,
while file reads and other irrelevant notifications leave it unchanged.

## Rendering contract

| Operation | Contract |
| --- | --- |
| Startup | Render the initial context before waiting for input events. |
| Rendering | Stage every output before replacing destinations. A rendering or staging failure leaves existing outputs untouched. |
| Replacement | Rename staged files individually. Each replacement is atomic on the same filesystem. A failed rename leaves earlier replacements in place and later outputs unchanged. |
| Recovery | The daemon logs rendering failures and continues watching. Staging-directory creation and output-replacement failures terminate the daemon. |
| Exit status | Successful one-shot rendering and shutdown through `SIGINT` or `SIGTERM` return `0`. One-shot errors and terminal daemon errors return `1`. |

Since later passes replace generated output, keep persistent changes in templates and configure
files and ignore generated paths in a dotfiles repository.

Applications can reload their configuration through filesystem watchers or systemd path units
that respond to generated file changes. These consumers receive notifications as individual files
are replaced, so reloads can occur while other files in the pass are still awaiting replacement.

## Profiles and merge policies

Top-level tables in configure files name profiles, which Figment composes in increasing order of
precedence from `default` through the selected profile to `global`. This lets a selected profile
override default values while values in `global` apply across profiles.

To assemble those profiles, Mirage evaluates candidate entries in manifest order, resolving each
`path` to a file or the current matches of a glob such as `profiles/**/*.toml`. Matches are discovered
again whenever the context is rebuilt. Each candidate's policy determines how its values combine
with the accumulated data, with all policies merging tables recursively.

| Policy | Figment operation | Conflicting scalars | Conflicting arrays |
| --- | --- | --- | --- |
| `override` | `merge` | Candidate wins | Candidate replaces existing array |
| `append` | `admerge` | Candidate wins | Existing and candidate arrays concatenated |
| `fallback` | `join` | Existing value wins | Existing array retained |
| `supplement` | `adjoin` | Existing value wins | Existing and candidate arrays concatenated |

When migrating a manifest from an earlier revision that interchanged `append` and `fallback`, swap
these policy names to preserve its previous merge results.

After combining the configure files, Mirage merges environment values prefixed with `MIRAGE_`.
The manifest itself accepts overrides through `MIRAGE_MANIFEST_`, whose merge concatenates arrays.

## Manifest settings

The `[configure]` table supplies the following settings, with command-line values taking precedence
where an option is available.

| Key | Default | Command-line option | Meaning |
| --- | --- | --- | --- |
| `profile` | `"default"` | `-P`, `--profile` | Initially selected Figment profile. |
| `template` | Required | `-T`, `--template` | Root containing templates and generated outputs. |
| `listen-sock` | `~/.mirage.sock` | `-L`, `--listen` | Daemon control socket. |
| `template-extension` | `"jinja"` | | Template extension, matched without regard to case. |
| `undefined` | `"strict"` | | Handling of missing template values. |
| `notificate-period` | `50` | | Quiet interval in milliseconds before rendering input changes. A value of `0` renders each relevant event immediately. |
| `module` | Omitted | | Luau entry module supplying template exports. |

The `undefined` setting controls how templates handle missing values. With `strict`, printing,
iteration, and truth tests fail, while `lenient` allows printing as an empty string and iteration
as an empty sequence. Nested lookups still fail under `lenient`, although `chainable` permits them
as well.

Paths can use a leading `~` and environment references written as `$VAR` or `${VAR}`. After
expansion, relative candidate paths and the Luau entry module resolve from the configure directory,
while relative template and socket paths resolve from the process's working directory.

Because manifest settings, candidate patterns, and environment overrides are loaded at startup,
changing them requires a daemon restart. Matching configure files and `.luau` modules reload while
the daemon is running.

## Luau extensions

To extend the template environment, set `module = "engine.luau"` in `[configure]` and create
`engine.luau` under the configure directory. Its return value supplies the functions, filters, and
globals to register with MiniJinja.

```lua
return {
    function_table = {
        upper = function(value) return tostring(value):upper() end,
    },
    filter_table = {
        shout = function(value) return tostring(value) .. "!" end,
    },
    environment_table = {
        brand = "MIRAGE",
    },
}
```

Mirage loads the module with Luau native code generation enabled and registers its exports, allowing
templates to use `{{ upper(brand) }}` and `{{ brand | shout }}`. Modules can import other files through
`require`, which resolves paths relative to the importing file and checks canonical paths against
the configure root. For directory imports, resolution tries `index.luau` before `init.luau`, and
successful imports remain cached for the lifetime of the VM.

Template arguments enter Luau as lazy value handles, where string keys access attributes and
integer keys use MiniJinja's zero-based sequence indices. The handles expose `:kind()`,
`:undefined()`, `:none()`, `:get(key)`, and `:materialize()`, with `:get(key)` providing access to data
keys that share a method name. To convert a value to native Luau data, call `:materialize()`.
Both the resulting sequences and generalized iteration over a handle use one-based Luau indices.

Access to Mirage host libraries is controlled by the `[module].libraries` allowlist. The available
library, `time`, supplies `require("@mirage/time").sleep(seconds)`, which an exported function can
call directly because the worker drives the asynchronous call to completion during rendering.
Enable it in the manifest with the following setting.

```toml
[module]
libraries = ["time"]
```

When a `.luau` file changes, Mirage rebuilds the VM and triggers rendering. If the reload fails,
the daemon logs the error and renders with the previous VM, while an error during initial module
loading propagates from startup.

## Runtime control

Runtime requests go through `miragectl`, which connects to `~/.mirage.sock` by default and prints
the daemon's JSON response. To address a daemon using another socket, select it with `-C` or
`--connect`.

```sh
miragectl -C /tmp/mirage.sock ping
```

| Command | Effect |
| --- | --- |
| `miragectl ping` | Check daemon responsiveness. |
| `miragectl profile powersave` | Select a profile and rebuild the context. |
| `miragectl mutate 'system.battery-state = "low"'` | Merge a transient TOML fragment into the current context. |
| `miragectl context` | Print the current render context inside a JSON response. |
| `miragectl hydrate` | Render the current context, including transient mutations. |

Mutations merge tables recursively and replace arrays and scalars in the live context, where they
remain until a configure-file or profile change rebuilds it. Since context rebuilding and rendering
proceed asynchronously, an acknowledgement confirms acceptance of the update or notification
without waiting for those operations to finish.

Mirage records daemon activity through `tracing` at `info` level by default, using `configure`,
`hydrate`, and `control` spans. For more detail, set `RUST_LOG=mirage=debug`.

## Testing

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
```

The CLI regression tests exercise initial rendering, filesystem reloads, and exit statuses by
launching daemon processes with temporary directories and Unix sockets. Unit tests cover the
underlying merge policies, Luau value conversion, module resolution, template loading, and output
replacement.

## Workspace

| Crate | Responsibility |
| --- | --- |
| [`mirage`](mirage) | Manifest loading, profile composition, Luau extensions, rendering, and daemon control. |
| [`miragectl`](miragectl) | Unix socket client for runtime control requests. |

## License

Copyright (C) 2026 W. Frakchi

Licensed under GPL-3.0-or-later. See [`LICENSE`](LICENSE).
