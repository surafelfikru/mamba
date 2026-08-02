//! The wire format between the `cargo` shim and the daemon.
//!
//! Length-prefixed frames over a unix socket, written against `std` alone. The shim runs
//! on every cargo invocation on the machine, so it stays free of serialization crates and
//! async runtimes — the few dozen lines here are cheaper than the dependency.
//!
//! An [`Invocation`] travels one way — this module only carries it, [`crate::input`]
//! decides what is in it. Frames travel back until a [`Frame::Exit`], which always ends
//! the conversation.

use crate::input::{Host, Invocation, ProjectId, ProjectRoot, Settings};
use std::io::{self, Read, Write};
use std::path::PathBuf;

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
    crate::input::mamba_home().join("daemon.sock")
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

pub fn write_invocation(out: &mut impl Write, invocation: &Invocation) -> io::Result<()> {
    out.write_all(&(invocation.flags.len() as u32).to_be_bytes())?;
    for flag in &invocation.flags {
        write_bytes(out, flag.as_bytes())?;
    }
    write_bytes(out, invocation.root.as_path().to_string_lossy().as_bytes())?;
    write_bytes(out, invocation.host.as_str().as_bytes())?;
    write_bytes(out, invocation.project_id.as_str().as_bytes())?;
    out.write_all(&[invocation.settings.bits()])?;
    out.flush()
}

/// Reads one invocation back, putting every value through the type that guards it.
///
/// The socket is one any process running as this user can open, and the host and project
/// id it carries go on to be pasted into ssh command lines. Rebuilding them through
/// [`Host::new`] and [`ProjectId::new`] rather than taking the strings as they arrive
/// makes this the last place that has to be checked, instead of every place they are used.
pub fn read_invocation(input: &mut impl Read) -> io::Result<Invocation> {
    let mut count = [0u8; 4];
    input.read_exact(&mut count)?;

    let mut flags = Vec::new();
    for _ in 0..u32::from_be_bytes(count) {
        flags.push(read_string(input)?);
    }

    let root = ProjectRoot::new(PathBuf::from(read_string(input)?));
    let host = Host::new(&read_string(input)?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let project_id = ProjectId::new(&read_string(input)?);

    let mut bits = [0u8; 1];
    input.read_exact(&mut bits)?;

    Ok(Invocation {
        root,
        host,
        project_id,
        settings: Settings::from_bits(bits[0]),
        flags,
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
            ));
        }
    };
    Ok(Some(frame))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::input::{PULL, SYMBOLS};

    /// The settings a command line asking for these flags would produce.
    fn asked_for(flags: &[&str]) -> Settings {
        Settings::from_flags(&flags.iter().map(|f| f.to_string()).collect::<Vec<_>>())
    }

    fn invocation() -> Invocation {
        Invocation {
            root: ProjectRoot::new(PathBuf::from("/home/dev/my proj")),
            host: Host::new("gpu-box").unwrap(),
            project_id: ProjectId::new("myproj"),
            settings: asked_for(&["--mamba-pull"]),
            flags: vec![
                "--release".to_string(),
                "--features".to_string(),
                "a b".to_string(),
            ],
        }
    }

    #[test]
    fn an_invocation_survives_the_round_trip() {
        let original = invocation();

        let mut buffer = Vec::new();
        write_invocation(&mut buffer, &original).unwrap();

        assert_eq!(read_invocation(&mut buffer.as_slice()).unwrap(), original);
    }

    #[test]
    fn a_host_that_a_shell_could_reinterpret_is_refused_on_the_way_in() {
        // Any process running as this user can write to the socket, and whatever arrives
        // here is pasted onto an ssh command line. The wire is where that gets checked.
        let mut buffer = Vec::new();
        write_invocation(&mut buffer, &invocation()).unwrap();

        // Overwrite the host with a shell injection of the same length.
        let hostile = b"gpu;id!";
        let at = buffer
            .windows(hostile.len())
            .position(|w| w == b"gpu-box")
            .expect("the host is in there");
        buffer[at..at + hostile.len()].copy_from_slice(hostile);

        assert!(read_invocation(&mut buffer.as_slice()).is_err());
    }

    #[test]
    fn the_settings_byte_is_read_back_as_the_same_set() {
        let mut original = invocation();
        original.settings = asked_for(&["--mamba-pull-symbols"]);

        let mut buffer = Vec::new();
        write_invocation(&mut buffer, &original).unwrap();
        let read = read_invocation(&mut buffer.as_slice()).unwrap();

        assert!(read.settings.has(SYMBOLS));
        assert!(
            read.settings.has(PULL),
            "symbols carry the binary with them"
        );
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

        let mut bare = invocation();
        bare.flags = Vec::new();
        let mut buffer = Vec::new();
        write_invocation(&mut buffer, &bare).unwrap();
        assert!(
            read_invocation(&mut buffer.as_slice())
                .unwrap()
                .flags
                .is_empty()
        );
    }

    #[test]
    fn an_unknown_frame_tag_is_an_error_rather_than_a_silent_skip() {
        let mut cursor: &[u8] = &[99, 0, 0, 0, 0];
        assert!(read_frame(&mut cursor).is_err());
    }
}
