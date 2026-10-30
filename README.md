# Mirage

Mirage keeps application configuration in sync with shared TOML data. You describe values in
profiles and use MiniJinja templates to turn those values into files. Run Mirage once to generate
the files, or leave its daemon running to regenerate them when inputs change. While it runs,
`miragectl` can select a different profile or update values in memory.

Each output lives beside its template, so `terminal.toml.jinja` produces `terminal.toml`. This lets
you keep templates in your application configuration directories while maintaining shared values
in one place. For transformations that templates alone cannot express, Luau modules can supply
functions, filters, and globals.

Mirage is under active development and is intended for Linux desktop configurations.

## Setup

Building Mirage requires the following tools and operating-system facilities.

- stable Rust and Cargo
- a C++17 toolchain for the bundled Luau runtime
- filesystem notification support and Unix domain sockets
- read access to configure files and templates
- write access to the template directories and control socket location

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

While the daemon is running, select the `powersave` profile from another terminal. Its background
overrides the default background, while the foreground remains `"#eeeeee"` because the selected
profile inherits values it does not define.

```sh
miragectl profile powersave
```

## Profiles and merge policies

Profiles let applications share defaults while changing selected values together. In the example,
switching to `powersave` changes the background wherever a template uses `theme.background`.
Additional candidate files can contribute values to those same profiles, allowing application
settings to live in separate files without giving up a shared profile selection.

Top-level tables in configure files name profiles. Their values combine in increasing order of
precedence from `default` through the selected profile to `global`. This lets a selected profile
override default values while values in `global` apply across profiles.

To assemble those profiles, Mirage evaluates candidate entries in manifest order, resolving each
`path` to a file or the current matches of a glob such as `profiles/**/*.toml`. Matches are discovered
again whenever the context is rebuilt. Each candidate's policy determines how its values combine
with the accumulated data, with all policies merging tables recursively.

| Policy | Conflicting scalars | Conflicting arrays |
| --- | --- | --- |
| `override` | Candidate wins | Candidate replaces existing array |
| `append` | Candidate wins | Existing and candidate arrays concatenated |
| `fallback` | Existing value wins | Existing array retained |
| `supplement` | Existing value wins | Existing and candidate arrays concatenated |

After combining the configure files, Mirage merges environment values prefixed with `MIRAGE_`.
The manifest itself accepts overrides through `MIRAGE_MANIFEST_`, whose merge concatenates arrays.

## Manifest settings

The `[configure]` table supplies the following settings, with command-line values taking precedence
where an option is available.

| Key | Default | Command-line option | Meaning |
| --- | --- | --- | --- |
| `profile` | `"default"` | `-P`, `--profile` | Initially selected profile. |
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

## Parametric templates

An ordinary template produces one file from the full context. When a table describes several
items, a parametric template can produce a file for each item instead. A path component such as
`{post}` selects each immediate child of the resolved `post` table and renders with that child
as its context.

For example, a small site can keep shared settings in one file and individual posts in others.
Start with this `.mirage.toml` in the project directory.

```toml
[configure]
template = "./templates"

[[candidate]]
path = "data/site.toml"
policy = "override"

[[candidate]]
path = "data/post/*.toml"
policy = "override"
```

Shared site settings go in `data/site.toml`.

```toml
[default.site]
title = "Example"
```

Each post contributes a child of `post`. Create `data/post/hello.toml` for the first article.

```toml
[default.post.hello]
title = "Hello World"
date = 2026-10-20
source = "content/hello.md"
```

A second file, `data/post/mirage.toml`, adds another child with an optional repository link.

```toml
[default.post.mirage]
title = "Building Mirage"
date = 2026-10-29
source = "content/mirage.md"
repository = "https://github.com/wafkse/mirage"
```

The template at `templates/post/{post}/index.html.jinja` renders those children using their own
fields. Quote paths containing braces when creating them through a shell.

```jinja
<h1>{{ title }}</h1>
<time>{{ date }}</time>
<p>Source: {{ source }}</p>
{% if repository is defined %}<a href="{{ repository }}">Repository</a>{% endif %}
```

Run `mirage --oneshot -C .`. This generates `templates/post/hello/index.html` and
`templates/post/mirage/index.html`, each rendered with its selected child's context. Ordinary
templates such as `templates/index.html.jinja` still receive the full context and can reference
`{{ site.title }}` or iterate over `post`.

To expand a nested table, use a dotted name such as `{catalog.items}`. With multiple parameters,
each selection becomes the context for the next. Thus `{group}/{items}/index.jinja` selects a group,
then that group's `items` children. Parameters must occupy entire path components after removal
of the template extension. Missing or non-table collections produce no instances, and child
values have no required schema. A template that accesses a missing value follows the configured
MiniJinja undefined behavior.

Because child keys become
output path components, they cannot be empty, `.` or `..`, or contain slashes, backslashes, colons,
or NUL. Before rendering, Mirage rejects duplicate destinations, file/directory conflicts,
template overwrites, and existing parent symlinks that escape the template root. These checks
assume the trusted local filesystem is not concurrently rearranged during a pass.

Mirage creates output directories as needed, but does not remove
outputs when a context key or template disappears. Removing an article therefore requires
explicitly deleting its stale generated files as well as its inputs. These outputs otherwise
follow the same update behavior as ordinary templates, described under
[rendering and updates](#rendering-and-updates).

## Runtime control

After the daemon starts, `miragectl` lets you inspect or change its live context without editing
the configure files. The client connects to `~/.mirage.sock` by default and prints
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

The daemon logs activity at `info` level by default. Set `RUST_LOG=mirage=debug` for more detail
when diagnosing configuration or rendering problems.

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

Templates can use the exported values as `{{ upper(brand) }}` and `{{ brand | shout }}`. Modules can import other files through
`require`, which resolves paths relative to the importing file during module evaluation.
Absolute imports and imports outside the configure root are allowed for trusted local code.
For directory imports, Mirage tries `index.luau` before `init.luau`. To apply changes to imported
files, follow the reload instructions below.

Template arguments enter Luau as lazy value handles, where string keys access attributes and
integer keys use MiniJinja's zero-based sequence indices. The handles expose `:kind()`,
`:undefined()`, `:none()`, `:get(key)`, and `:materialize()`, with `:get(key)` providing access to data
keys that share a method name. To convert a value to native Luau data, call `:materialize()`.
Both the resulting sequences and generalized iteration over a handle use one-based Luau indices.

Values returned to a template follow the reverse conversion. Nested Luau tables become MiniJinja
maps and sequences, with empty tables treated as empty sequences. Cycles are rejected and nesting
is limited to 128 levels, so exported functions must return acyclic data within that limit.

Access to Mirage host libraries is controlled by the `[module].libraries` allowlist. The `time`
library supplies `require("@mirage/time").sleep(seconds)`, which pauses an exported function
for the specified duration.
Enable it in the manifest with the following setting.

```toml
[module]
libraries = ["time"]
```

The `fs` library provides `require("@mirage/fs").read(path)`, which returns a file's bytes as a
Luau string and raises an error on I/O failure. Relative paths resolve from the configure directory.
Absolute paths are used directly. Reads follow symlinks and can access any file readable by the
process, including files outside the configure directory. Enable it with `[module] libraries =
["fs"]` (or `["fs", "time"]` to enable both libraries). This allowlist makes host API usage explicit.
It does not make trusted Luau code a filesystem sandbox.

Changes to files read through `fs.read` do not automatically trigger rendering. After editing
such a file, run `miragectl hydrate`, run Mirage again in one-shot mode, or arrange an external
watcher to request rendering.

Changes to `.luau` files inside the configure directory reload the extensions and render again.
If reloading fails, Mirage logs the error and keeps using the previous extensions. An error in
the initial module prevents startup. Imported modules outside the configure directory require
touching the configured entry module or restarting Mirage to reload them. `miragectl hydrate`
alone does not reload imported modules.

## Rendering Markdown

The host libraries can work together to turn content files into template data. Enable
`libraries = ["fs", "markdown"]` under `[module]` to read Markdown with `@mirage/fs` and parse
it with `@mirage/markdown`. The Markdown module performs no file reads of its own.

Its `parse(string)` function returns a document tree for templates to present.
Each node carries `type` and `children`, with `literal` holding text on literal nodes. Headings add `level`,
links and images add `destination` and `title`, code blocks add `language`, and lists add
`ordered` with an optional `start`. Tables, strikethrough, task markers, and footnotes are enabled.
Raw HTML remains literal `html` nodes so templates control whether to render or escape it.

For example, configure `module = "lua/engine.luau"` and create that file.

```lua
local fs = require("@mirage/fs")
local markdown = require("@mirage/markdown")

return {
    function_table = {
        document = function(source)
            return markdown.parse(fs.read(source:materialize()))
        end,
    },
}
```

Calling `document(source)` from a template passes the source path as a lazy value handle. The
function materializes that path for `fs.read`, then returns the parsed tree. Mirage converts
the nested Luau tables into MiniJinja maps and sequences, making the nodes available to a
recursive macro. The following macro renders text and headings, escaping text as HTML.

```jinja
{% macro render_node(node) -%}
  {%- if node.type == "text" -%}
    {{- node.literal | e -}}
  {%- elif node.type == "heading" -%}
    <h{{ node.level }}>{% for child in node.children %}{{ render_node(child) }}{% endfor %}</h{{ node.level }}>
  {%- else -%}
    {%- for child in node.children -%}{{ render_node(child) }}{%- endfor -%}
  {%- endif -%}
{%- endmacro %}
{{ render_node(document(source)) }}
```

Other node types need their own presentation branches. As with other filesystem reads, changes
to Markdown sources require an explicit hydration request or another render trigger.

## Rendering and updates

Mirage finds templates recursively, including in dot directories, and renders them when it
starts. Each subsequent update renders all templates again. The output name drops the template
extension, and the generated file keeps the template's permissions. Built-in MiniJinja functions
and filters, including `tojson`, are available alongside any Luau exports.

While the daemon runs, edits to matching configure files, templates, and local Luau modules
trigger updates. The `notificate-period` setting lets a burst of edits settle before rendering.
Changes to the manifest or environment require restarting the daemon.

If rendering fails, existing output files remain unchanged and the daemon keeps watching for
further edits. Successful renders replace files individually, so applications watching multiple
outputs may briefly see values from different updates. A replacement failure can leave some
files updated and others unchanged, and terminates the daemon. A failure to prepare temporary
output storage also terminates it.

Applications can reload through their own filesystem watchers or systemd path units. Keep
persistent changes in templates and configure files because later renders replace generated
content. In a dotfiles repository, ignore generated paths to avoid tracking those replacements.

Successful one-shot rendering and shutdown through `SIGINT` or `SIGTERM` return exit status `0`.
One-shot errors and terminal daemon errors return `1`, allowing scripts and service managers
to detect a failed run.

## License

Copyright (C) 2026 W. Frakchi

Licensed under GPL-3.0-or-later. See [`LICENSE`](LICENSE).
