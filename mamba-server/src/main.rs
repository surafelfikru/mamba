//! The Mamba build server.
//!
//! Optional. Mamba works against any host reachable over ssh without it; running this
//! adds a control plane that owns its own filesystem placement and can later dispatch to
//! machines the client never learns about.
//!
//! ```sh
//! mamba-server --root ~/.mamba/projects --port 7777 --advertise-host gpu-box
//! ```
//!
//! It listens without authentication or encryption. Run it only on a trusted network.

mod service;

use mamba_core::proto::control_server::ControlServer;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let mut root = home.join(".mamba/projects");
    let mut port: u16 = 7777;
    let mut advertise_host = "localhost".to_string();
    let mut advertise_user = std::env::var("USER").unwrap_or_else(|_| "root".to_string());

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let value = args.get(i + 1).cloned();
        match args[i].as_str() {
            "--root" => root = PathBuf::from(value.ok_or("--root needs a path")?),
            "--port" => port = value.ok_or("--port needs a number")?.parse()?,
            "--advertise-host" => advertise_host = value.ok_or("--advertise-host needs a name")?,
            "--advertise-user" => advertise_user = value.ok_or("--advertise-user needs a name")?,
            other => return Err(format!("unknown flag {other}").into()),
        }
        i += 2;
    }

    std::fs::create_dir_all(&root)?;
    let addr = format!("0.0.0.0:{port}").parse()?;
    let service = service::ControlService::new(root.clone(), advertise_host, advertise_user);

    eprintln!("mamba-server: serving {} on {addr}", root.display());
    tonic::transport::Server::builder()
        .add_service(ControlServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
