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

`cargo build` is intercepted and run on a machine you pick, with the compiler output
streaming back to your terminal. Source is pushed with rsync, so only what changed moves;
the remote `target/` directory stays put and acts as the build cache.

Builds go through a small local daemon that keeps its connection to the build host open
between builds, rather than opening a fresh ssh connection per step.

Nothing needs installing on the build host — an ordinary ssh destination with a Rust
toolchain is enough. If a `mamba-server` happens to be listening on port 7777, Mamba uses
that instead, which lets the host decide where projects live and where builds run. Which
transport was chosen appears in the `Syncing` line of every build.

`mamba-server` currently accepts connections without authentication or encryption. Run it
only on a network you trust. The plain ssh path is unaffected — it is encrypted and
authenticated as it always was.

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
pull = false                  # optional; fetch the built binary after a successful build
pull-symbols = false          # optional; also fetch its debug symbols
```

`host` is the only setting that names the build machine. Where the project lands over
there is not your decision to make any more — the build host answers that, and the
project is identified by its directory name.

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
cargo build --mamba-pull            # this build only
cargo build --mamba-pull-symbols    # also fetch debug symbols (implies pull)
```

```toml
pull = true                     # every build in this project
pull-symbols = true             # symbols too — and, again, implies pull
```

Asking for symbols asks for the binary as well, in either place: the stripped binary
carries the link a debugger follows to find them, so one without the other is no use.
The two sources add up rather than override — a switch is on if either turns it on, which
is what makes a flag win over a config file that left it off. It's prefixed `--mamba-pull`
rather than `--pull`
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

The binary you get is stripped of debug info, which makes it roughly four times
smaller to transfer. Panic backtraces still show function names. For line numbers
and source-level debugging, add `--mamba-pull-symbols` to fetch the symbol file — your
debugger picks it up automatically once it is there.

If the remote is unreachable, Mamba asks whether to build locally instead. With no
terminal to ask on it falls back automatically and says so. A compile error is never
treated as a network problem, so a failing build never silently recompiles locally.

Two projects with the same directory name share a remote build directory, because the
directory name is what identifies a project. Rename one if that affects you.

