mod app;
mod session;

use std::path::PathBuf;

use app::App;
use clap::{Parser, ValueEnum};
use session::{SessionManager, SessionTarget};
use waypane_transport::{Authentication, HostKeyPolicy, RemoteWaypipe, SshConfig};
use waypipe_client::{ClientConfig, Compression};
use winit::event_loop::EventLoop;

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum CompressionArg {
    None,
    #[default]
    Lz4,
    Zstd,
}

impl From<CompressionArg> for Compression {
    fn from(value: CompressionArg) -> Self {
        match value {
            CompressionArg::None => Compression::None,
            CompressionArg::Lz4 => Compression::Lz4,
            CompressionArg::Zstd => Compression::Zstd,
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Display remote Wayland applications as native windows via Waypipe")]
struct Args {
    #[arg(long, conflicts_with = "ssh")]
    socket: Option<PathBuf>,

    #[arg(long, value_name = "[USER@]HOST[:PORT]", conflicts_with = "socket")]
    ssh: Option<String>,

    #[arg(long)]
    user: Option<String>,

    #[arg(long)]
    port: Option<u16>,

    #[arg(long)]
    identity: Option<PathBuf>,

    #[arg(long)]
    passphrase_env: Option<String>,

    #[arg(long)]
    password_env: Option<String>,

    #[arg(long)]
    known_hosts: Option<PathBuf>,

    #[arg(long)]
    accept_new_host_key: bool,

    #[arg(long, default_value = "waypipe")]
    waypipe_bin: String,

    #[arg(long, value_name = "PATH")]
    remote_socket: Option<String>,

    #[arg(long, value_enum, default_value_t)]
    compression: CompressionArg,

    #[arg(long, hide = true)]
    exit_after_frames: Option<usize>,

    #[arg(last = true)]
    command: Vec<String>,
}

fn env_value(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("environment variable {name} is not set"))
}

fn parse_endpoint(target: &str) -> Result<(Option<String>, String, u16), String> {
    let (user, address) = target
        .rsplit_once('@')
        .map_or((None, target), |(user, address)| {
            (Some(user.to_owned()), address)
        });
    if let Some(rest) = address.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .ok_or_else(|| "IPv6 endpoint must use [address]:port".to_string())?;
        let port = port
            .parse::<u16>()
            .map_err(|_| "invalid SSH port".to_string())?;
        return Ok((user, host.to_owned(), port));
    }
    if let Some((host, port)) = address.rsplit_once(':') {
        if !host.contains(':') {
            let port = port
                .parse::<u16>()
                .map_err(|_| "invalid SSH port".to_string())?;
            return Ok((user, host.to_owned(), port));
        }
    }
    Ok((user, address.to_owned(), 22))
}

fn session_target(args: &Args) -> Result<SessionTarget, String> {
    let compression = Compression::from(args.compression);
    if let Some(socket) = &args.socket {
        let _ = compression;
        return Ok(SessionTarget::Local(socket.clone()));
    }
    let target = args
        .ssh
        .as_deref()
        .ok_or_else(|| "either --socket or --ssh is required".to_string())?;
    if args.command.is_empty() {
        return Err("missing remote command after --".to_string());
    }
    let (endpoint_user, host, endpoint_port) = parse_endpoint(target)?;
    let user = args
        .user
        .clone()
        .or(endpoint_user)
        .ok_or_else(|| "SSH user is required (use USER@HOST or --user)".to_string())?;
    let authentication = if let Some(name) = &args.password_env {
        Authentication::Password(env_value(name)?)
    } else if let Some(path) = &args.identity {
        Authentication::PublicKey {
            path: path.clone(),
            passphrase: args.passphrase_env.as_deref().map(env_value).transpose()?,
        }
    } else {
        Authentication::Auto
    };
    let mut remote = RemoteWaypipe::new(compression, args.command.clone())
        .map_err(|error| error.to_string())?
        .with_binary(args.waypipe_bin.clone());
    if let Some(path) = &args.remote_socket {
        remote = remote
            .with_remote_socket(path.clone())
            .map_err(|error| error.to_string())?;
    }
    let host_key_policy = if args.accept_new_host_key {
        HostKeyPolicy::AcceptNew
    } else {
        HostKeyPolicy::Check
    };
    Ok(SessionTarget::Ssh {
        config: Box::new(SshConfig {
            host,
            port: args.port.unwrap_or(endpoint_port),
            user,
            authentication,
            known_hosts: args.known_hosts.clone(),
            host_key_policy,
        }),
        remote: Box::new(remote),
    })
}

fn main() -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();
    let target = session_target(&args).map_err(|error| format!("argument error: {error}"))?;
    let compression = Compression::from(args.compression);
    let event_loop = EventLoop::<session::SessionEvent>::with_user_event().build()?;
    let manager = SessionManager::start(target, event_loop.create_proxy());
    let mut app = App::new(
        manager,
        ClientConfig {
            preferred_compression: compression,
            ..ClientConfig::default()
        },
        args.exit_after_frames,
    );
    event_loop.run_app(&mut app)?;
    Ok(std::process::ExitCode::from(app.exit_code()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_endpoints() {
        assert_eq!(
            parse_endpoint("alice@example.test:2222").unwrap(),
            (Some("alice".into()), "example.test".into(), 2222)
        );
        assert_eq!(
            parse_endpoint("root@[::1]:8022").unwrap(),
            (Some("root".into()), "::1".into(), 8022)
        );
        assert_eq!(
            parse_endpoint("localhost").unwrap(),
            (None, "localhost".into(), 22)
        );
    }
}
