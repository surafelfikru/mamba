//! The gRPC implementation of [`ControlChannel`].
//!
//! The only file in the client that knows tonic exists. It translates the daemon's three
//! questions into RPCs and transport failures into [`ChannelError`], which the daemon
//! treats as a reason to fall back to ssh.
//!
//! This connection carries no credentials and no encryption.

use crate::channel::{BuildStream, ChannelError, ControlChannel};
use async_trait::async_trait;
use mamba_core::proto::artifact_request::Kind;
use mamba_core::proto::control_client::ControlClient;
use mamba_core::proto::{ArtifactRequest, BuildRequest, TransferTarget, UploadRequest};
use tokio_stream::StreamExt;
use tonic::transport::Channel;

/// Where a `mamba-server` is expected to listen.
///
/// Fixed rather than configured: a user sets a hostname and nothing else, so the daemon
/// has to know where to look without being told.
pub const GRPC_PORT: u16 = 7777;

pub struct GrpcControlChannel {
    client: ControlClient<Channel>,
}

impl GrpcControlChannel {
    /// Opens a connection, failing rather than retrying — the daemon decides what to do
    /// about a server that is not there, not this layer.
    pub async fn connect(uri: &str) -> Result<GrpcControlChannel, ChannelError> {
        let client = ControlClient::connect(uri.to_string())
            .await
            .map_err(|e| ChannelError(format!("could not reach {uri}: {e}")))?;
        Ok(GrpcControlChannel { client })
    }
}

#[async_trait]
impl ControlChannel for GrpcControlChannel {
    async fn request_upload(&self, project_id: &str) -> Result<TransferTarget, ChannelError> {
        let mut client = self.client.clone();
        client
            .request_upload(UploadRequest {
                project_id: project_id.to_string(),
            })
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChannelError(format!("upload target refused: {e}")))
    }

    async fn start_build(
        &self,
        project_id: &str,
        args: &[String],
        local_root: &str,
    ) -> Result<BuildStream, ChannelError> {
        let mut client = self.client.clone();
        let stream = client
            .start_build(BuildRequest {
                project_id: project_id.to_string(),
                args: args.to_vec(),
                local_root: local_root.to_string(),
            })
            .await
            .map_err(|e| ChannelError(format!("build refused: {e}")))?
            .into_inner();

        Ok(Box::pin(stream.map(|event| {
            event.map_err(|e| ChannelError(format!("build stream broke: {e}")))
        })))
    }

    async fn request_artifact(
        &self,
        project_id: &str,
        kind: Kind,
    ) -> Result<TransferTarget, ChannelError> {
        let mut client = self.client.clone();
        client
            .request_artifact(ArtifactRequest {
                project_id: project_id.to_string(),
                kind: kind as i32,
            })
            .await
            .map(|r| r.into_inner())
            .map_err(|e| ChannelError(format!("artifact unavailable: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connecting_where_nothing_listens_is_an_error_not_a_panic() {
        // Port 1 never has a mamba-server. This must surface as ChannelError so the
        // daemon falls back to ssh rather than failing the build.
        assert!(
            GrpcControlChannel::connect("http://127.0.0.1:1")
                .await
                .is_err()
        );
    }

    #[test]
    fn the_probe_port_is_fixed_because_it_is_never_configured() {
        assert_eq!(GRPC_PORT, 7777);
    }
}
