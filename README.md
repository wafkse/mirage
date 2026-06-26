# Mirage

Mirage is a lightweight, asynchronous background daemon that orchestrates your dotfiles using a single source of truth.

Managing a customized Linux desktop involves juggling overlapping design tokens: colors, fonts, borders and layout gaps, across tools that use entirely different configuration formats. Your terminal might use TOML, your window manager a custom syntax, your status bar JSON, and your app launcher CSS. Trying to keep these in sync usually means writing fragile `sed` scripts or adopting massive framework wrappers like Home Manager.

Mirage solves this by completely separating your **data** from your **layout**. You define your variables in central TOML files, write your configurations as minijinja templates, and let Mirage hydrate everything on the fly. 

It acts as a reactive state engine for your filesystem.

### Extreme Resource Efficiency

Mirage is designed to run continuously as a background process. Written in async Rust, it spends almost its entire lifecycle asleep, blocked on filesystem (`inotify`) and socket (`epoll`) syscalls. This is to make multiple Mirage instances running across the system not take a noticeable amount of memory. 

## Core Concepts

Mirage revolves around two primary concepts: **Configure Candidates** (the data) and **Template Candidates** (the layouts).

### Configure Candidates
Configure candidates are standard TOML files that contain your system's variables. These are the single source of truth for your environment. 

Mirage maintains an active filesystem watch on all loaded configure candidates. **If a single TOML candidate is modified, Mirage instantly recalculates the entire state tree and forces a complete re-render of all tracked templates.** This guarantees that your environment is never out of sync.

### Template Candidates
Template candidates are the actual configuration files for your applications, written using minijinja (Jinja2-compatible) templating syntax. Mirage recursively watches the target directory defined in your manifest for these files.

Mirage identifies render targets by scanning for any file ending in the configured template extension, `.jinja` by default. You write your configuration files exactly as you normally would, injecting variables where needed (e.g., `{{ theme.base_color }}` or `{{ layout.gaps_in }}`).

The extension is configurable with the `template_extension` key under `[configure]` (set it to `"tera"` to migrate a legacy tree without renaming). Undefined references are a hard render error by default; the `undefined` key (`"strict"`, `"lenient"`, or `"chainable"`) relaxes this.

Filesystem watchers emit several notifications for a single logical edit (a rename, a data write, a metadata touch). Mirage coalesces such a burst into one render by holding the pass back for a short grace period, configurable with the `notificate-period` key under `[configure]` (milliseconds, defaulting to `50`). Setting it to `0` renders eagerly on every notification.

## The Manifest: Bringing It Together

To make Mirage work, you must define a `.mirage.toml` manifest file. This manifest tells the daemon where to find your data, where to output your templates, and how to resolve conflicts between your TOML files.

A typical manifest looks like this:

```
[configure]
profile = "default"
template = "/home/user/.config"
listen_sock = "/home/user/.mirage.sock"

[[candidate]]
path = "base.toml"
policy = "override"

[[candidate]]
path = "**/[!.]*.toml"
policy = "override"
```

### Candidate Evaluation Order
Mirage evaluates the `[[candidate]]` blocks sequentially, from top to bottom. The list is locked in at daemon startup, meaning your evaluation order is strictly deterministic. If multiple TOML files define the same variable, Mirage uses the order of these candidates to decide which value wins.

### Merge Policies
Mirage utilizes the `figment` configuration library under the hood. When it processes your candidates, it applies the `policy` you specify to handle conflicts. The supported merge policies are:

* **`override`**: The standard behavior. Values in later candidates completely replace values from earlier candidates.
* **`append`**: Arrays and collections are concatenated together instead of being overwritten.
* **`fallback`**: The candidate only provides values for keys that have not already been defined by an earlier candidate.
* **`supplement`**: Similar to fallback, but applies specifically to appending arrays without overriding existing entries.

## Scripting with Luau

Mirage ships no template functions of its own. Every template function and filter is authored in Luau and supplied per deployment by a module the manifest points at via the `module` key under `[configure]` (resolved relative to the configure root, mirroring `require`).

The module returns a table of what it exports:

```lua
return {
    functions = { upper = function(value) return tostring(value):upper() end },
    filters   = { shout = function(value) return tostring(value) .. "!" end },
    globals   = { brand = "MIRAGE" },
}
```

`functions` and `filters` become template functions and filters under their keys, and `globals` become constant template globals. Module code receives template values as opaque, lazily-resolving handles: indexing walks the underlying value tree (`value.a.b[1]`) without materializing it, and `:kind()`, `:is_undefined()`, `:is_none()`, `:get(key)`, and `:to_lua()` are available for inspection. A change to module code rebuilds the VM and forces a full re-render.

The Luau VM and the rendering engine live on a dedicated hydration worker with its own runtime. On top of the vanilla Luau standard library, Mirage exposes its own modules under the `@mirage/<lib>` require convention, each gated by an explicit `[module].libraries` allowlist. Module functions may be asynchronous — for example, with `@mirage/time` allowlisted, `require("@mirage/time").sleep(seconds)` can be `await`ed and the worker drives it to completion within the otherwise-synchronous render. A module evaluation error aborts the render pass under the same all-or-nothing contract as a template error.

## Profile & Variable Semantics

Inside your TOML configure candidates, variables are organized into tables. Mirage treats these top-level tables as **Profiles**, which allows you to define conditional states (like a "powersave" mode or a "dark" theme) alongside your base configurations.

When Mirage builds its internal context, it resolves variables based on strict overriding semantics:

1. **`[default]`**: The foundational layer. If a variable is not defined anywhere else, Mirage uses the value found in the `[default]` block.
2. **`[<profile_name>]`**: The currently active profile layer (e.g., `[performance]`). Variables declared here override the fallback defaults for the current session.
3. **`[global]`**: The absolute override layer. Variables placed in the `[global]` block supersede all other blocks, regardless of which profile is currently active.

This layered approach allows you to set up a massive `[default]` configuration across your files, and then define a tiny `[powersave]` profile that only overrides the variables necessary to disable compositor blur and animations.

## The Hydration Engine

When state changes, Mirage executes a strict, transactional pipeline to update your files.

### All-or-Nothing Rendering
To ensure your desktop never ends up in a broken or half-configured state, Mirage treats every state change as a transaction. 
When a re-render is triggered, Mirage evaluates *all* templates into a temporary directory on the same filesystem mount. If a single template fails to compile, whether due to a syntax error, a missing variable, or an invalid type, the entire hydration pass aborts. Your existing, working configurations are left completely untouched, and the error is logged.

### In-Place, Atomic Renames

Mirage does not use a separate "output" directory or a complex web of symlinks. It renders in-place.

When processing `<filename>.<extension>.jinja`, the compiled output is written directly adjacent to the source file as `<filename>.<extension>`. Mirage copies the original file permissions and applies the new file using an atomic `rename` syscall. 

Because the replacement is atomic, external applications watching those files via `inotify` will never read a partial write or an empty file.

### One-Shot Rendering

Some workflows want a single hydration pass rather than a resident daemon: a login script, a `Makefile` target, or a `git` hook. Passing `--oneshot` folds the manifest, renders every template once under the same all-or-nothing contract, and exits without starting the daemon, binding the control socket, or establishing any filesystem watch.

```bash
# Render the manifest once and exit
mirage --oneshot -C ~/.config/mirage
```

### Deferred Side-Effects

Certain applications require a Unix signal or shell command to reload their configuration. To support this, Mirage allows templates to register post-hydration shell commands via custom template functions. 

To prevent race conditions, these functions do not execute immediately when the template is evaluated. Instead, they append commands to an internal queue. Mirage flushes and executes this side-effect queue only *after* the atomic rename phase successfully commits. The application is never instructed to reload before its configuration is safely written to disk.

## The Runtime Control Plane (`miragectl`)

Mirage listens on a Unix Domain Socket (defined in the manifest, defaulting to `~/.mirage.sock`). The companion utility, `miragectl`, leverages this socket to mutate the daemon's internal state dynamically. This design enables deep integration with window manager keybinds, udev rules, or shell scripts.

### Profile Toggling
You can hot-swap massive blocks of variables instantly by telling the daemon to switch its active profile. This forces an immediate recalculation of the state tree and a full system re-render.
```bash
# Switch the entire desktop to a performance state
miragectl --profile "performance"
```

### Ephemeral State Mutation
You can inject specific variables directly into the daemon's memory space without modifying your TOML files. This is highly effective for updating hardware metrics or handling dynamic states that shouldn't be permanently saved to disk.
```bash
# Inject a dynamic variable into the state tree
miragectl --mutate 'system.battery-state = "low"'
```

## Operational Disclaimers

Mirage is unopinionated power-user software. It delegates filesystem organization entirely to the user and expects you to manage your own repository hygiene.

A teeny bit of personal recommendations:

* You should add a negative `.gitignore` line to not include any non-Tera file into the untracked file pool if your dotfiles are being managed via Git: `!*.tera`. This keeps hydrated artifacts out of the repository.
* Do not edit non-Tera hydrated files directly, any re-hydration will override all your changes. The template dictates all.

# License

Copyright (C) 2026 W. Frakchi

This program is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version.

See [the full license agreement](./LICENSE) for further information.
