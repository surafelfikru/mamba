//! The daemon's view of wherever a build happens.
//!
//! Everything the daemon needs from the far side is these three calls, so the daemon
//! depends on this trait rather than on any particular protocol. Two implementations ship:
//! [`crate::ssh::SshControlChannel`], which needs no server at all, and
//! [`crate::grpc::GrpcControlChannel`], which talks to `mamba-server`.
//!
//! ```
//! let target = channel.request_upload("myproj").await?;
//! ```

use async_trait::async_trait;
use mamba_core::proto::artifact_request::Kind;
use mamba_core::proto::{BuildEvent, TransferTarget};
use std::pin::Pin;
use tokio_stream::Stream;

/// A stream of build output frames, ending with one exit-code frame.
pub type BuildStream = Pin<Box<dyn Stream<Item = Result<BuildEvent, ChannelError>> + Send>>;

/// Something went wrong reaching or talking to the far side. Never a compile error —
/// those arrive as an exit code inside a perfectly healthy stream.
#[derive(Debug)]
pub struct ChannelError(pub String);

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[async_trait]
pub trait ControlChannel: Send + Sync {
    /// Where this project's source should be pushed.
    async fn request_upload(&self, project_id: &str) -> Result<TransferTarget, ChannelError>;

    /// Runs a build, streaming its output.
    async fn start_build(
        &self,
        project_id: &str,
        args: &[String],
        local_root: &str,
    ) -> Result<BuildStream, ChannelError>;

    /// Where one artifact of the last build ended up.
    async fn request_artifact(
        &self,
        project_id: &str,
        kind: Kind,
    ) -> Result<TransferTarget, ChannelError>;
}

#[cfg(test)]
use mamba_core::proto::build_event;

#[cfg(test)]
/// An in-memory channel for testing the daemon without a server or a network.
///
/// It advertises a host deliberately unlike any control endpoint, which is what the
/// daemon's routing test relies on: a transfer target must be obeyed exactly as given.
pub struct MockChannel {
    host: String,
}

#[cfg(test)]
impl MockChannel {
    pub fn new(host: &str) -> MockChannel {
        MockChannel {
            host: host.to_string(),
        }
    }

    fn target(&self, relative_path: &str) -> TransferTarget {
        TransferTarget {
            host: self.host.clone(),
            port: 22,
            user: "builder".to_string(),
            path: format!("/srv/{relative_path}"),
            relative_path: relative_path.to_string(),
        }
    }

    /// Drains a build stream into a vector. Test helper.
    pub async fn collect_build(
        &self,
        project_id: &str,
        args: &[String],
        local_root: &str,
    ) -> Result<Vec<BuildEvent>, ChannelError> {
        use tokio_stream::StreamExt;
        let mut stream = self.start_build(project_id, args, local_root).await?;
        let mut all = Vec::new();
        while let Some(event) = stream.next().await {
            all.push(event?);
        }
        Ok(all)
    }
}

#[cfg(test)]
#[async_trait]
impl ControlChannel for MockChannel {
    // build_event is only needed by this test double.
    async fn request_upload(&self, _project_id: &str) -> Result<TransferTarget, ChannelError> {
        Ok(self.target(""))
    }

    async fn start_build(
        &self,
        _project_id: &str,
        _args: &[String],
        _local_root: &str,
    ) -> Result<BuildStream, ChannelError> {
        let events = vec![
            Ok(BuildEvent {
                payload: Some(build_event::Payload::Stderr(b"   Compiling app\n".to_vec())),
            }),
            Ok(BuildEvent {
                payload: Some(build_event::Payload::ExitCode(0)),
            }),
        ];
        Ok(Box::pin(tokio_stream::iter(events)))
    }

    async fn request_artifact(
        &self,
        _project_id: &str,
        kind: Kind,
    ) -> Result<TransferTarget, ChannelError> {
        let rel = match kind {
            Kind::Binary => "target/debug/app",
            Kind::Symbols => "target/debug/app.debug",
            Kind::ProcMacros => "target/debug/deps",
            Kind::GeneratedSource => "target/debug/build",
        };
        Ok(self.target(rel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_mock_channel_answers_all_three_questions() {
        let mock = MockChannel::new("worker-7");

        assert_eq!(mock.request_upload("proj").await.unwrap().host, "worker-7");

        let art = mock.request_artifact("proj", Kind::Binary).await.unwrap();
        assert_eq!(art.host, "worker-7");
        assert_eq!(art.relative_path, "target/debug/app");

        let events = mock.collect_build("proj", &[], "/tmp/proj").await.unwrap();
        assert!(matches!(
            events.last(),
            Some(BuildEvent {
                payload: Some(build_event::Payload::ExitCode(0))
            })
        ));
    }
}
