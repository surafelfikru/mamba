//! The wire format between the `cargo` shim and the daemon.
//!
//! Length-prefixed frames over a unix socket, written against `std` alone. The shim runs
//! on every cargo invocation on the machine, so it stays free of serialization crates and
//! async runtimes — the few dozen lines here are cheaper than the dependency.
//!
//! A request travels one way. Frames travel back until a [`Frame::Exit`], which always
//! ends the conversation.

use std::io::{self, Read, Write};
use std::path::PathBuf;

/// Everything the daemon needs to run one build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Cargo's own arguments, without the `build` subcommand.
    pub args: Vec<String>,
    pub root: String,
    pub host: String,
    pub project_id: String,
    pub pull: bool,
    pub symbols: bool,
}

/// One message from the daemon back to the shim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// A cargo-style status line: a verb and a message.
    Status(String, String),
    /// The build never ran. The shim offers a local build when it sees this.
    Failed(String),
    /// The build's own exit code. Always the last frame.
    Exit(i32),
}

/// Where the daemon listens.
pub fn socket_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".mamba").join("daemon.sock")
}

fn write_bytes(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    out.write_all(&(bytes.len() as u32).to_be_bytes())?;
    out.write_all(bytes)
}

fn read_bytes(input: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    input.read_exact(&mut len)?;
    let mut buffer = vec![0u8; u32::from_be_bytes(len) as usize];
    input.read_exact(&mut buffer)?;
    Ok(buffer)
}

fn read_string(input: &mut impl Read) -> io::Result<String> {
    String::from_utf8(read_bytes(input)?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_request(out: &mut impl Write, request: &Request) -> io::Result<()> {
    out.write_all(&(request.args.len() as u32).to_be_bytes())?;
    for arg in &request.args {
        write_bytes(out, arg.as_bytes())?;
    }
    write_bytes(out, request.root.as_bytes())?;
    write_bytes(out, request.host.as_bytes())?;
    write_bytes(out, request.project_id.as_bytes())?;
    out.write_all(&[u8::from(request.pull) | (u8::from(request.symbols) << 1)])?;
    out.flush()
}

pub fn read_request(input: &mut impl Read) -> io::Result<Request> {
    let mut count = [0u8; 4];
    input.read_exact(&mut count)?;

    let mut args = Vec::new();
    for _ in 0..u32::from_be_bytes(count) {
        args.push(read_string(input)?);
    }

    let root = read_string(input)?;
    let host = read_string(input)?;
    let project_id = read_string(input)?;

    let mut flags = [0u8; 1];
    input.read_exact(&mut flags)?;

    Ok(Request {
        args,
        root,
        host,
        project_id,
        pull: flags[0] & 1 != 0,
        symbols: flags[0] & 2 != 0,
    })
}

pub fn write_frame(out: &mut impl Write, frame: &Frame) -> io::Result<()> {
    match frame {
        Frame::Stdout(b) => {
            out.write_all(&[1])?;
            write_bytes(out, b)?;
        }
        Frame::Stderr(b) => {
            out.write_all(&[2])?;
            write_bytes(out, b)?;
        }
        Frame::Status(verb, message) => {
            out.write_all(&[3])?;
            write_bytes(out, verb.as_bytes())?;
            write_bytes(out, message.as_bytes())?;
        }
        Frame::Failed(why) => {
            out.write_all(&[4])?;
            write_bytes(out, why.as_bytes())?;
        }
        Frame::Exit(code) => {
            out.write_all(&[5])?;
            out.write_all(&code.to_be_bytes())?;
        }
    }
    out.flush()
}

/// Reads one frame, or `None` when the sender closed cleanly.
pub fn read_frame(input: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut tag = [0u8; 1];
    match input.read_exact(&mut tag) {
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
        Ok(()) => {}
    }

    let frame = match tag[0] {
        1 => Frame::Stdout(read_bytes(input)?),
        2 => Frame::Stderr(read_bytes(input)?),
        3 => Frame::Status(read_string(input)?, read_string(input)?),
        4 => Frame::Failed(read_string(input)?),
        5 => {
            let mut code = [0u8; 4];
            input.read_exact(&mut code)?;
            Frame::Exit(i32::from_be_bytes(code))
        }
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown frame tag {other}"),
            ))
        }
    };
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_survives_the_round_trip() {
        let original = Request {
            args: vec![
                "--release".to_string(),
                "--features".to_string(),
                "a b".to_string(),
            ],
            root: "/home/dev/my proj".to_string(),
            host: "gpu-box".to_string(),
            project_id: "myproj".to_string(),
            pull: true,
            symbols: false,
        };

        let mut buffer = Vec::new();
        write_request(&mut buffer, &original).unwrap();

        assert_eq!(read_request(&mut buffer.as_slice()).unwrap(), original);
    }

    #[test]
    fn every_frame_kind_survives_the_round_trip() {
        let frames = vec![
            Frame::Stdout(b"compiling\n".to_vec()),
            Frame::Stderr(b"warning\n".to_vec()),
            Frame::Status("Downloading".to_string(), "app".to_string()),
            Frame::Failed("host unreachable".to_string()),
            Frame::Exit(101),
        ];

        let mut buffer = Vec::new();
        for frame in &frames {
            write_frame(&mut buffer, frame).unwrap();
        }

        let mut cursor = buffer.as_slice();
        for expected in &frames {
            assert_eq!(&read_frame(&mut cursor).unwrap().unwrap(), expected);
        }
        assert!(
            read_frame(&mut cursor).unwrap().is_none(),
            "the stream must end cleanly"
        );
    }

    #[test]
    fn empty_payloads_and_empty_argument_lists_are_legal() {
        let mut buffer = Vec::new();
        write_frame(&mut buffer, &Frame::Stdout(Vec::new())).unwrap();
        assert_eq!(
            read_frame(&mut buffer.as_slice()).unwrap().unwrap(),
            Frame::Stdout(Vec::new())
        );

        let request = Request {
            args: Vec::new(),
            root: String::new(),
            host: String::new(),
            project_id: String::new(),
            pull: false,
            symbols: false,
        };
        let mut buffer = Vec::new();
        write_request(&mut buffer, &request).unwrap();
        assert!(read_request(&mut buffer.as_slice()).unwrap().args.is_empty());
    }

    #[test]
    fn an_unknown_frame_tag_is_an_error_rather_than_a_silent_skip() {
        let mut cursor: &[u8] = &[99, 0, 0, 0, 0];
        assert!(read_frame(&mut cursor).is_err());
    }
}
