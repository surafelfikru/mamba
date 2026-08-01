//! Everything the two ends of Mamba have to agree on.
//!
//! The wire contract lives in [`proto`]; the rules for where cargo puts its output live in
//! `layout`. Both are here rather than on one side because both sides need them: the
//! server resolves paths for the gRPC transport, and the SSH transport resolves the same
//! paths locally because it has no server to ask.

pub mod proto {
    tonic::include_proto!("mamba.control.v1");
}

#[cfg(test)]
mod tests {
    use super::proto::*;

    #[test]
    fn every_message_the_service_needs_exists_and_round_trips_its_fields() {
        let target = TransferTarget {
            host: "gpu-box".to_string(),
            port: 22,
            user: "ubuntu".to_string(),
            path: "/srv/builds/proj/target/debug/app.slim".to_string(),
            relative_path: "target/debug/app".to_string(),
        };
        assert_eq!(target.relative_path, "target/debug/app");

        let request = ArtifactRequest {
            project_id: "proj".to_string(),
            kind: artifact_request::Kind::ProcMacros as i32,
        };
        assert_eq!(
            artifact_request::Kind::try_from(request.kind).unwrap(),
            artifact_request::Kind::ProcMacros
        );
    }

    #[test]
    fn a_build_event_carries_exactly_one_of_output_or_an_exit_code() {
        let out = BuildEvent {
            payload: Some(build_event::Payload::Stdout(b"hi".to_vec())),
        };
        let done = BuildEvent {
            payload: Some(build_event::Payload::ExitCode(101)),
        };

        assert!(matches!(out.payload, Some(build_event::Payload::Stdout(_))));
        assert!(matches!(
            done.payload,
            Some(build_event::Payload::ExitCode(101))
        ));
    }
}
