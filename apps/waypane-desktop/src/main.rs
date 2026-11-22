mod app;
#[cfg(unix)]
mod openssh;
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
#[command(
    about = "Display remote Wayland applications as native windows via Waypipe",
    long_about = "Display remote Wayland applications as native windows via Waypipe.\n\nWith --ssh and no -- <COMMAND>, macOS and Unix hosts start a system OpenSSH login shell (using ~/.ssh/config and SSH agents) and forward every Wayland program launched from it. An explicit -- <COMMAND> uses the built-in russh transport without a PTY."
)]
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

fn parse_endpoint(target: &str) -> Result<(Option<String>, String, Option<u16>), String> {
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
        return Ok((user, host.to_owned(), Some(port)));
    }
    if let Some((host, port)) = address.rsplit_once(':') {
        if !host.contains(':') {
            let port = port
                .parse::<u16>()
                .map_err(|_| "invalid SSH port".to_string())?;
            return Ok((user, host.to_owned(), Some(port)));
        }
    }
    Ok((user, address.to_owned(), None))
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
            port: args.port.or(endpoint_port).unwrap_or(22),
            user,
            authentication,
            known_hosts: args.known_hosts.clone(),
            host_key_policy,
        }),
        remote: Box::new(remote),
    })
}

#[cfg(unix)]
fn interactive_options(args: &Args) -> Result<openssh::OpenSshOptions, String> {
    if args.password_env.is_some() || args.passphrase_env.is_some() {
        return Err(
            "--password-env and --passphrase-env are only supported with an explicit -- <COMMAND>; \
             interactive OpenSSH mode uses ssh-agent, ~/.ssh/config, and OpenSSH's own prompts"
                .to_string(),
        );
    }
    let (endpoint_user, host, endpoint_port) = parse_endpoint(
        args.ssh
            .as_deref()
            .ok_or_else(|| "--ssh is required for interactive mode".to_string())?,
    )?;
    let destination = match args.user.clone().or(endpoint_user) {
        Some(user) => format!("{user}@{host}"),
        None => host,
    };
    let mut remote =
        RemoteWaypipe::login_shell(args.compression.into()).with_binary(args.waypipe_bin.clone());
    if let Some(path) = &args.remote_socket {
        remote = remote
            .with_remote_socket(path.clone())
            .map_err(|error| error.to_string())?;
    }
    Ok(openssh::OpenSshOptions {
        destination,
        port: args.port.or(endpoint_port),
        identity: args.identity.clone(),
        known_hosts: args.known_hosts.clone(),
        accept_new_host_key: args.accept_new_host_key,
        remote,
    })
}

fn main() -> Result<std::process::ExitCode, Box<dyn std::error::Error>> {
    let args = Args::parse();
    let interactive = args.ssh.is_some() && args.command.is_empty();
    let compression = Compression::from(args.compression);
    let event_loop = EventLoop::<session::SessionEvent>::with_user_event().build()?;
    #[cfg(unix)]
    let mut interactive_session = None;

    let target = if interactive {
        #[cfg(unix)]
        {
            let options =
                interactive_options(&args).map_err(|error| format!("argument error: {error}"))?;
            match openssh::start(options, event_loop.create_proxy()) {
                Ok((target, session)) => {
                    interactive_session = Some(session);
                    target
                }
                Err(error) => return Err(error.into()),
            }
        }
        #[cfg(not(unix))]
        {
            return Err(
                "interactive OpenSSH shells are only supported on macOS/Unix; pass an explicit -- <COMMAND> on Windows"
                    .into(),
            );
        }
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
        session_target(&args).map_err(|error| format!("argument error: {error}"))?
    };

    let exit_when_idle = if interactive {
        session::ExitWhenIdle::No
    } else {
        session::ExitWhenIdle::Yes
    };
    let manager = SessionManager::start(target, event_loop.create_proxy(), exit_when_idle);
    let mut app = App::new(
        manager,
        ClientConfig {
            preferred_compression: compression,
            ..ClientConfig::default()
        },
        args.exit_after_frames,
        exit_when_idle,
    );
    let result = event_loop.run_app(&mut app);
    #[cfg(unix)]
    drop(interactive_session);
    result?;
    Ok(std::process::ExitCode::from(app.exit_code()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn interactive_mode_uses_config_aliases_and_rejects_environment_secrets() {
        let args = Args::parse_from(["waypane-desktop", "--ssh", "workstation"]);
        let options = interactive_options(&args).unwrap();
        assert_eq!(options.destination, "workstation");
        assert_eq!(options.port, None);
        assert!(options.remote.is_login_shell());

        let args = Args::parse_from([
            "waypane-desktop",
            "--ssh",
            "workstation",
            "--password-env",
            "WAYPANE_PASSWORD",
        ]);
        assert!(interactive_options(&args)
            .unwrap_err()
            .contains("only supported with an explicit -- <COMMAND>"));

        let args = Args::parse_from([
            "waypane-desktop",
            "--ssh",
            "otherhost:2222",
            "--user",
            "alice",
        ]);
        let options = interactive_options(&args).unwrap();
        assert_eq!(options.destination, "alice@otherhost");
        assert_eq!(options.port, Some(2222));
    }

    #[test]
    fn parses_ssh_endpoints() {
        assert_eq!(
            parse_endpoint("alice@example.test:2222").unwrap(),
            (Some("alice".into()), "example.test".into(), Some(2222))
        );
        assert_eq!(
            parse_endpoint("root@[::1]:8022").unwrap(),
            (Some("root".into()), "::1".into(), Some(8022))
        );
        assert_eq!(
            parse_endpoint("localhost").unwrap(),
            (None, "localhost".into(), None)
        );
    }
}
