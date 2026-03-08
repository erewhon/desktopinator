use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

use anyhow::Context;
use dinator_ipc::{IpcCommand, IpcEvent, IpcResponse};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty() {
        print_usage();
        std::process::exit(1);
    }

    let command = parse_command(&args)?;
    let is_subscribe = matches!(command, IpcCommand::Subscribe);

    let socket_path = dinator_ipc::socket_path();
    let mut stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("failed to connect to {}", socket_path.display()))?;

    // Send command as JSON line
    let mut msg = serde_json::to_string(&command)?;
    msg.push('\n');
    stream.write_all(msg.as_bytes())?;

    // Read response
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;

    let response: IpcResponse = serde_json::from_str(line.trim())
        .context("failed to parse compositor response")?;

    match &response {
        IpcResponse::Ok { message } => {
            if let Some(msg) = message {
                if !is_subscribe {
                    println!("{msg}");
                }
            }
        }
        IpcResponse::Data { data } => {
            println!("{}", serde_json::to_string_pretty(data)?);
        }
        IpcResponse::Error { message } => {
            eprintln!("error: {message}");
            std::process::exit(1);
        }
    }

    // In subscribe mode, stream events until disconnected
    if is_subscribe {
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line)?;
            if n == 0 {
                break; // compositor closed connection
            }
            let event: IpcEvent = serde_json::from_str(line.trim())
                .context("failed to parse event")?;
            println!("{}", serde_json::to_string(&event)?);
        }
    }

    Ok(())
}

fn parse_command(args: &[String]) -> anyhow::Result<IpcCommand> {
    let cmd = args[0].as_str();
    match cmd {
        "resize" => {
            if args.len() != 2 {
                anyhow::bail!("usage: dinatorctl resize WIDTHxHEIGHT");
            }
            let parts: Vec<&str> = args[1].split('x').collect();
            if parts.len() != 2 {
                anyhow::bail!("resolution must be WIDTHxHEIGHT (e.g. 1920x1080)");
            }
            let width: u16 = parts[0].parse().context("invalid width")?;
            let height: u16 = parts[1].parse().context("invalid height")?;
            Ok(IpcCommand::Resize { width, height })
        }
        "focus-next" | "focus_next" => Ok(IpcCommand::FocusNext),
        "focus-prev" | "focus_prev" => Ok(IpcCommand::FocusPrev),
        "close" => Ok(IpcCommand::Close),
        "swap-master" | "swap_master" => Ok(IpcCommand::SwapMaster),
        "spawn" => {
            if args.len() < 2 {
                anyhow::bail!("usage: dinatorctl spawn COMMAND [ARGS...]");
            }
            Ok(IpcCommand::Spawn {
                cmd: args[1].clone(),
                args: args[2..].to_vec(),
            })
        }
        "quit" => Ok(IpcCommand::Quit),
        "list-windows" | "list_windows" | "windows" => Ok(IpcCommand::ListWindows),
        "subscribe" | "events" => Ok(IpcCommand::Subscribe),
        _ => {
            anyhow::bail!("unknown command: {cmd}\n\nRun 'dinatorctl' with no args for usage.");
        }
    }
}

fn print_usage() {
    eprintln!(
        "dinatorctl — control the desktopinator compositor

USAGE:
    dinatorctl <command> [args...]

COMMANDS:
    resize WxH          Change output resolution (e.g. 1920x1080)
    focus-next           Focus the next window
    focus-prev           Focus the previous window
    close                Close the focused window
    swap-master          Swap focused window with master
    spawn CMD [ARGS]     Launch a program
    list-windows         List all managed windows
    subscribe            Stream compositor events (JSON lines)
    quit                 Quit the compositor"
    );
}
