# Mirage

Mirage is an asynchronous background daemon that keeps your dotfiles in sync from a single source of truth.

A customized Linux desktop spreads the same handful of design tokens (colors, fonts, borders, gaps) across tools that all speak different configuration formats. Your terminal reads TOML, your window manager a custom syntax, your status bar JSON, your launcher CSS. Keeping them consistent usually comes down to fragile `sed` scripts or a large framework like Home Manager.

Mirage keeps the data separate from the layout. You put your variables in central TOML files, write each application's config as a minijinja template, and let the daemon render them whenever the data changes.

## Resource use

Mirage is meant to run continuously in the background. It is written in async Rust and spends nearly all of its time asleep, blocked on filesystem (`inotify`) and socket (`epoll`) syscalls. Running several instances across a system therefore costs very little memory.

## Core concepts

Mirage has two kinds of input: configure candidates (the data) and template candidates (the layouts).

### Configure candidates

Configure candidates are plain TOML files holding your variables. They are the source of truth for the environment.

Mirage watches every loaded candidate. When one is modified it recomputes the whole state tree and re-renders all tracked templates, so the output never drifts from the data.

### Template candidates

Template candidates are your application configs, written in minijinja (Jinja2-compatible) syntax. Mirage recursively watches the target directory from the manifest and treats any file ending in the configured template extension (`.jinja` by default) as a render target. You write the config as usual and inject variables where you need them, for example `{{ theme.base_color }}` or `{{ layout.gaps_in }}`.

The extension comes from the `template-extension` key under `[configure]`; whatever you pick, the same minijinja engine renders it. Undefined references abort the render by default. The `undefined` key (`"strict"`, `"lenient"`, or `"chainable"`) loosens that.

A single edit often makes the filesystem emit several notifications: a rename, then a data write, then a metadata touch. Mirage holds the render back for a short grace period to coalesce the burst into one pass. That period is the `notificate-period` key under `[configure]`, in milliseconds, defaulting to `50`; set it to `0` to render on every notification.

## The manifest

Mirage is configured by a `.mirage.toml` manifest. It tells the daemon where the data lives, where to write output, and how to resolve conflicts between TOML files.

```toml
[configure]
profile = "default"

# Where hydrated templates live (or pass -T on the CLI).
template = "/home/user/.config"

# The Unix socket miragectl connects to (or pass -L on the CLI).
listen-sock = "/home/user/.mirage.sock"

# Luau module supplying template functions/filters/globals, resolved
# relative to the configure root like `require` (dir -> index.luau).
module = "engine.luau"

# Undefined-reference behaviour: "strict" (default), "lenient", or
# "chainable". Strict aborts the pass on any missing value.
undefined = "strict"

[[candidate]]
path = "base.toml"
policy = "override"

[[candidate]]
path = "**/[!.]*.toml"
policy = "override"
```

### Candidate order

Mirage reads the `[[candidate]]` blocks top to bottom, and the list is fixed at startup, so the order is deterministic. When several TOML files set the same variable, that order decides which value wins.

### Merge policies

Merging runs on the `figment` library. Each candidate's `policy` controls how it combines with what came before:

* `override`: later candidates replace earlier values. This is the usual choice.
* `append`: arrays are concatenated instead of replaced.
* `fallback`: the candidate only fills in keys no earlier candidate defined.
* `supplement`: like `fallback`, but for appending to arrays without touching existing entries.

## Scripting with Luau

Apart from minijinja's own built-ins (the standard filters, plus the `json` feature's `tojson` for serializing a value tree to JSON), Mirage provides no template helpers of its own. Every domain-specific function and filter is written in Luau, in a module the manifest names with the `module` key under `[configure]`. The path resolves relative to the configure root the way `require` does.

The module returns a table of its exports:

```lua
return {
    function_table = { upper = function(value) return tostring(value):upper() end },
    filter_table   = { shout = function(value) return tostring(value) .. "!" end },
    environment_table   = { brand = "MIRAGE" },
}
```

Entries in `function_table` and `filter_table` register as template functions and filters under their keys, and `environment_table` supplies constant globals. Module code sees template values as opaque, lazily-resolving handles: indexing walks the underlying tree (`value.a.b[1]`) without materializing it, and `:kind()`, `:undefined()`, `:none()`, `:get(key)`, and `:materialize()` are there for inspection. Editing the module rebuilds the VM and forces a full re-render.

The Luau VM and the render engine share a dedicated hydration worker with its own runtime. Alongside the standard Luau library, Mirage exposes its own modules under the `@mirage/<lib>` require convention, each gated by the `[module].libraries` allowlist. A module function may be asynchronous: with `@mirage/time` allowlisted, `require("@mirage/time").sleep(seconds)` can be `await`ed, and the worker drives it to completion inside the otherwise synchronous render. An error while evaluating the module aborts the pass under the same all-or-nothing rule as a template error.

## Profiles and variables

Variables in the configure candidates live in top-level TOML tables. Mirage reads those tables as profiles, which lets you keep conditional states (a "powersave" mode, a "dark" theme) next to your base configuration. It resolves a variable through three layers:

1. `[default]`: the base. Used when nothing else defines the variable.
2. `[<profile>]`: the active profile, say `[performance]`. Overrides the default for the current session.
3. `[global]`: the top override. Beats every other block no matter which profile is active.

So you can write one large `[default]` across your files and add a small `[powersave]` that only overrides what it needs, like turning off compositor blur and animations.

## The hydration engine

A state change runs a transactional pipeline.

### All-or-nothing rendering

So the desktop never lands in a half-written state, each change is one transaction. Mirage renders every template into a temporary directory on the same filesystem mount. If any template fails, whether from a syntax error, a missing variable, or a bad type, the whole pass aborts, your current configs stay as they are, and the error is logged.

### In-place atomic renames

Mirage renders in place. There is no separate output directory and no symlink web. For `<filename>.<extension>.jinja` it writes the result next to the source as `<filename>.<extension>`, copies the original permissions, and swaps it in with an atomic `rename`. Because the swap is atomic, anything watching the file through `inotify` never sees a partial write or an empty file.

### One-shot rendering

Some workflows want one pass instead of a resident daemon: a login script, a `Makefile` target, a `git` hook. `--oneshot` folds the manifest, renders every template once under the same all-or-nothing rule, and exits without starting the daemon, binding the socket, or watching the filesystem.

```bash
# Render the manifest once and exit
mirage --oneshot -C ~/.config/mirage
```

### Deferred side effects

Some applications need a signal or a shell command before they pick up a new config. Templates can queue such commands through a helper function rather than running them on the spot. Mirage flushes the queue only after the atomic rename commits, so an application is never told to reload before its config is on disk.

## Runtime control (`miragectl`)

Mirage listens on a Unix domain socket, set in the manifest and defaulting to `~/.mirage.sock`. The `miragectl` utility talks to that socket to change the daemon's state at runtime, which is handy from window manager keybinds, udev rules, or shell scripts.

The commands are subcommands, and `-C`/`--connect <socket>` points at a specific daemon. `ping` and `hydrate` check liveness and force a re-render, `profile` and `mutate` reshape the context, and `context` dumps the daemon's resolved state.

### Switching profiles

Swap a whole block of variables by changing the active profile. This recomputes the state tree and re-renders everything.

```bash
# Switch the entire desktop to a performance state
miragectl profile "performance"
```

### Transient mutation

Inject a variable straight into the running daemon without editing any TOML. This suits hardware metrics and other dynamic state you would not want to persist.

```bash
# Inject a dynamic variable into the state tree
miragectl mutate 'system.battery-state = "low"'
```

## Notes

Mirage is unopinionated. It leaves filesystem layout to you and assumes you manage your own repository hygiene. A few suggestions:

* If your dotfiles are in Git, add a negative ignore for templates so hydrated output stays out of the repo, for example `!*.jinja`.
* Do not edit hydrated files by hand. The next render overwrites them; the template is the only source.
* Mirage logs through `tracing` at info level by default. Set `RUST_LOG` (for example `RUST_LOG=mirage=debug`) to raise it, with per-subsystem spans (`configure`, `hydrate`, `control`) around each event.

# License

Copyright (C) 2026 W. Frakchi

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

See [the full license agreement](./LICENSE) for further information.
