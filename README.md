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

