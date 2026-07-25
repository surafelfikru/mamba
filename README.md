# Mamba

Mamba is a remote rust build tool that is opinionated. It makes `cargo build` faster by moving compilation to remote machines of your choice and eagerly reusing shared build artifacts.

Rust compilation is powerful but can be slow, especially on laptops and large projects. even with abundant resources rust compilation can use up a lot of disk space through registry caches and incremental builds.

Developers repeatedly compile the same dependencies, waste CPU time, and wait for builds instead of writing code. Mamba reduces build times by providing faster compute and intelligent caching. It uses industry standard tools and multi layer cache orchestration to bring optimal results on remote servers.

## How

A lightweight Cargo integration sends builds to remote workers.

The platform uses:

- Cargo-aware build orchestration
- Dedicated build workers
- `sccache` for artifact reuse
- Local NVMe hot caches
- Object storage for cold artifacts
- Containerized isolated environments

## What works today

The first slice is in: `cargo build` is intercepted and run over ssh on a machine you
pick, with the compiler output streaming back to your terminal. Source is pushed with
rsync, so only what changed moves; the remote `target/` directory stays put and acts as
the build cache.

`sccache`, object storage, containers, and dedicated workers are not built yet.

### Requirements

`rsync` and `ssh` locally, `rsync` and `cargo` on the remote machine.

Cargo needs to work under a plain non-interactive ssh session, i.e.
`ssh host cargo --version` should print a version. A standard rustup install
satisfies this on its own — Mamba sources `~/.cargo/env` itself before running
`cargo build`, so you don't need to touch the remote's `.bashrc`/`.zshrc`.

### Install

```sh
cargo install --path .
ln -s "$(command -v mamba)" ~/.local/bin/cargo
```

`~/.local/bin` must come before `~/.cargo/bin` on your `PATH`. Mamba installs itself as
a symlink named `cargo`, which is how interception works without shell aliases — so it
also covers `make`, build scripts, and rust-analyzer.

### Use

Create `.mamba.toml` in any project you want built remotely:

```toml
host = "gpu-box"              # any ssh destination, or an alias from ~/.ssh/config
remote_dir = ".mamba/myproj"  # optional; relative paths sit in the remote home dir
pull = false                  # optional; see "Pulling the binary back" below
```

That file is the entire opt-in. Projects without one are untouched.

```sh
cargo build --release    # compiles on gpu-box
cargo test               # still local
```

Put ports, usernames, keys, and jump hosts in `~/.ssh/config` — `host` accepts an alias
from it, so Mamba has no separate ssh settings of its own.

### Pulling the binary back

By default nothing comes back — see "Limits worth knowing" below for why. To fetch the
built binary to the exact local path cargo would have used (`target/debug/yourbin` or
`target/release/yourbin`), either pass a flag or set it in the config:

```sh
cargo build --mamba-pull        # this build only
```

```toml
pull = true                     # every build in this project
```

The flag always wins if both are set. It's prefixed `--mamba-pull` rather than `--pull`
so it can never collide with a real (current or future) cargo flag — Mamba strips it
before anything reaches cargo, local or remote. Only the crate's default binary (the one
matching `[package] name` under `src/main.rs`) is resolved; a workspace or an explicit
`[[bin]]` target under a different name isn't. Pulling only happens after a successful
build (exit 0) — a compile error leaves nothing on the remote worth fetching, and the
pull step never changes the build's own exit code, only stderr gets a line either way.

Whether this is worth it depends on the binary's size and the network's latency to your
host — for reference, a 4.3MB debug binary over a ~120ms-RTT link took about 4 extra
seconds. On a large binary or a high-latency link that cost adds up fast; measure before
turning it on by default.

### Limits worth knowing

Only `cargo build` goes remote. Every other subcommand runs locally, untouched.

Build artifacts stay on the remote machine unless you opt into pulling them (above) —
`./target/debug/yourbin` will not exist locally after a plain remote build.

If the remote is unreachable, Mamba asks whether to build locally instead. With no
terminal to ask on it falls back automatically and says so. A compile error is never
treated as a network problem, so a failing build never silently recompiles locally.

Two projects with the same directory name share a default `remote_dir`. Set `remote_dir`
explicitly if that affects you.

