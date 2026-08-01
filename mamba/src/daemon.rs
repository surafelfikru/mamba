//! The resident half of Mamba.
//!
//! One process per user, started by the shim the first time a build needs it and left
//! running afterwards. Its whole job is holding connections open: a control channel per
//! host and, underneath rsync, one multiplexed ssh connection. Between builds it does
//! nothing — no polling, only a slow sweep for channels nobody has used in ten minutes.
//!
//! It never decides where anything lives on the far side. It asks, then obeys.

use crate::channel::ControlChannel;
use crate::grpc::{GrpcControlChannel, GRPC_PORT};
use crate::ipc::{self, Frame, Request};
use crate::ssh::SshControlChannel;
use crate::transfer;
use mamba_core::proto::artifact_request::Kind;
use mamba_core::proto::build_event;
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{UnixListener, UnixStream};
use tokio_stream::StreamExt;

/// How long a channel survives with nothing using it. The daemon itself stays up.
const IDLE_TIMEOUT: Duration = Duration::from_secs(600);

/// How long to wait for a `mamba-server` before falling back to plain ssh.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Open channels, keyed by host, with the transport's name and when each was last used.
pub struct ChannelCache {
    entries: HashMap<String, (Arc<dyn ControlChannel>, &'static str, Instant)>,
    timeout: Duration,
}

impl ChannelCache {
    pub fn new(timeout: Duration) -> ChannelCache {
        ChannelCache {
            entries: HashMap::new(),
            timeout,
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn insert(
        &mut self,
        host: &str,
        channel: Arc<dyn ControlChannel>,
        transport: &'static str,
    ) {
        self.entries
            .insert(host.to_string(), (channel, transport, Instant::now()));
    }

    pub fn get(&mut self, host: &str) -> Option<(Arc<dyn ControlChannel>, &'static str)> {
        let entry = self.entries.get_mut(host)?;
        entry.2 = Instant::now();
        Some((Arc::clone(&entry.0), entry.1))
    }

    /// Drops channels nobody has used inside the timeout.
    pub fn evict_idle(&mut self, now: Instant) {
        let timeout = self.timeout;
        self.entries
            .retain(|_, (_, _, last)| now.duration_since(*last) < timeout);
    }
}

/// Picks a transport for a host: `mamba-server` if one answers, plain ssh otherwise.
///
/// The probe is what lets a user configure nothing but a hostname. A short timeout keeps
/// the common case — no server — from delaying the build noticeably, and the result is
/// cached, so this runs once per host per ten minutes rather than once per build.
async fn open_channel(host: &str) -> (Arc<dyn ControlChannel>, &'static str) {
    let uri = format!("http://{host}:{GRPC_PORT}");

    match tokio::time::timeout(PROBE_TIMEOUT, GrpcControlChannel::connect(&uri)).await {
        Ok(Ok(channel)) => (Arc::new(channel), "grpc"),
        _ => (Arc::new(SshControlChannel::new(host)), "ssh"),
    }
}

/// Runs one build to completion, writing frames back as they happen.
///
/// The order is fixed: ask where to push, push, build, then ask for each artifact wanted.
/// Editor artifacts come down after every successful build because rust-analyzer needs
/// them whether or not anyone asked for the binary.
pub async fn handle(
    request: &Request,
    channel: &dyn ControlChannel,
    out: &mut impl Write,
    transport: &str,
) -> io::Result<()> {
    let root = Path::new(&request.root);

    let upload = match channel.request_upload(&request.project_id).await {
        Ok(t) => t,
        Err(e) => return ipc::write_frame(out, &Frame::Failed(e.to_string())),
    };

    ipc::write_frame(
        out,
        &Frame::Status(
            "Syncing".to_string(),
            format!("{} to {} via {transport}", request.project_id, upload.host),
        ),
    )?;
    if let Err(e) = transfer::push(root, &upload) {
        return ipc::write_frame(out, &Frame::Failed(e));
    }

    let mut stream = match channel
        .start_build(&request.project_id, &request.args, &request.root)
        .await
    {
        Ok(s) => s,
        Err(e) => return ipc::write_frame(out, &Frame::Failed(e.to_string())),
    };

    let mut exit = 1;
    while let Some(event) = stream.next().await {
        match event {
            Err(e) => return ipc::write_frame(out, &Frame::Failed(e.to_string())),
            Ok(event) => match event.payload {
                Some(build_event::Payload::Stdout(b)) => ipc::write_frame(out, &Frame::Stdout(b))?,
                Some(build_event::Payload::Stderr(b)) => ipc::write_frame(out, &Frame::Stderr(b))?,
                Some(build_event::Payload::ExitCode(c)) => exit = c,
                None => {}
            },
        }
    }

    if exit == 0 {
        let mut wanted = vec![Kind::ProcMacros, Kind::GeneratedSource];
        if request.pull {
            wanted.push(Kind::Binary);
        }
        if request.symbols {
            wanted.push(Kind::Symbols);
        }

        for kind in wanted {
            // A failed fetch is reported and forgotten. The build already succeeded, and
            // its exit code is the answer the caller asked for.
            match channel.request_artifact(&request.project_id, kind).await {
                Err(e) => {
                    ipc::write_frame(out, &Frame::Stderr(format!("mamba: {e}\n").into_bytes()))?
                }
                Ok(target) => match transfer::fetch(root, &target, kind) {
                    Err(e) => ipc::write_frame(
                        out,
                        &Frame::Stderr(format!("mamba: fetch failed: {e}\n").into_bytes()),
                    )?,
                    Ok(path) => {
                        if matches!(kind, Kind::Binary | Kind::Symbols) {
                            ipc::write_frame(
                                out,
                                &Frame::Status(
                                    "Downloaded".to_string(),
                                    path.display().to_string(),
                                ),
                            )?;
                        }
                    }
                },
            }
        }
    }

    ipc::write_frame(out, &Frame::Exit(exit))
}

/// Serves the socket until killed.
///
/// Binding is how two daemons are prevented: if the socket already answers, another daemon
/// won the race and this one exits quietly. If it exists but is dead, it is a leftover
/// from a killed process and gets removed.
pub fn run() -> std::process::ExitCode {
    let path = ipc::socket_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    if std::os::unix::net::UnixStream::connect(&path).is_ok() {
        return std::process::ExitCode::SUCCESS;
    }
    let _ = std::fs::remove_file(&path);

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mamba: could not start the daemon runtime: {e}");
            return std::process::ExitCode::from(1);
        }
    };

    runtime.block_on(async move {
        let listener = match UnixListener::bind(&path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("mamba: could not bind {}: {e}", path.display());
                return std::process::ExitCode::from(1);
            }
        };

        let cache = Arc::new(tokio::sync::Mutex::new(ChannelCache::new(IDLE_TIMEOUT)));

        let sweeper = Arc::clone(&cache);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(60));
            loop {
                ticker.tick().await;
                sweeper.lock().await.evict_idle(Instant::now());
            }
        });

        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let cache = Arc::clone(&cache);
            tokio::spawn(async move {
                let _ = serve_one(stream, cache).await;
            });
        }
    })
}

/// Reads one request off a connection, runs it, and writes the frames back.
async fn serve_one(
    stream: UnixStream,
    cache: Arc<tokio::sync::Mutex<ChannelCache>>,
) -> io::Result<()> {
    let stream = stream.into_std()?;
    stream.set_nonblocking(false)?;
    let mut reader = stream.try_clone()?;
    let request = ipc::read_request(&mut reader)?;
    let mut writer = stream;

    let cached = cache.lock().await.get(&request.host);
    let (channel, transport) = match cached {
        Some(pair) => pair,
        None => {
            let (channel, transport) = open_channel(&request.host).await;
            cache
                .lock()
                .await
                .insert(&request.host, Arc::clone(&channel), transport);
            (channel, transport)
        }
    };

    handle(&request, channel.as_ref(), &mut writer, transport).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel::MockChannel;

    fn request(pull: bool) -> Request {
        Request {
            args: Vec::new(),
            root: "/tmp/does-not-matter".to_string(),
            host: "gpu-box".to_string(),
            project_id: "proj".to_string(),
            pull,
            symbols: false,
        }
    }

    fn frames(bytes: &[u8]) -> Vec<Frame> {
        let mut cursor = bytes;
        let mut all = Vec::new();
        while let Some(frame) = crate::ipc::read_frame(&mut cursor).unwrap() {
            all.push(frame);
        }
        all
    }

    #[tokio::test]
    async fn the_chosen_transport_is_announced_rather_than_silent() {
        let mut out = Vec::new();
        // The push fails against a host that does not exist, so this asserts on what
        // arrives before it: the status line naming the transport. That line is the whole
        // mitigation for a probe that silently falls back — without it, a server merely
        // being down is invisible.
        handle(&request(false), &MockChannel::new("worker-7"), &mut out, "mock")
            .await
            .unwrap();

        let frames = frames(&out);
        assert!(
            frames.iter().any(
                |f| matches!(f, Frame::Status(verb, msg) if verb == "Syncing" && msg.contains("mock"))
            ),
            "the chosen transport must be announced: {frames:?}"
        );
        assert!(
            frames.iter().any(|f| matches!(f, Frame::Failed(_))),
            "a push that cannot happen must fail the build, not hang: {frames:?}"
        );
    }

    #[tokio::test]
    async fn an_idle_channel_is_dropped_but_a_fresh_one_survives() {
        let mut cache = ChannelCache::new(Duration::from_secs(600));

        cache.insert("gpu-box", Arc::new(MockChannel::new("w")), "mock");
        assert_eq!(cache.len(), 1);

        cache.evict_idle(Instant::now());
        assert_eq!(cache.len(), 1, "a channel just used must survive");

        cache.evict_idle(Instant::now() + Duration::from_secs(601));
        assert_eq!(cache.len(), 0, "an idle channel must be dropped");
    }

    #[tokio::test]
    async fn a_cached_channel_is_reused_rather_than_reconnected() {
        let mut cache = ChannelCache::new(Duration::from_secs(600));
        cache.insert("gpu-box", Arc::new(MockChannel::new("w")), "mock");

        let (_, transport) = cache.get("gpu-box").expect("the channel must still be there");

        assert_eq!(transport, "mock");
    }
}
