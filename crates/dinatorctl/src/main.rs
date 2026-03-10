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
        "master-grow" | "master_grow" => Ok(IpcCommand::MasterGrow),
        "master-shrink" | "master_shrink" => Ok(IpcCommand::MasterShrink),
        "layout" | "set-layout" | "set_layout" => {
            if args.len() != 2 {
                anyhow::bail!("usage: dinatorctl layout <column|monocle>");
            }
            Ok(IpcCommand::SetLayout { name: args[1].clone() })
        }
        "toggle-float" | "toggle_float" | "float" => Ok(IpcCommand::ToggleFloat),
        "toggle-fullscreen" | "toggle_fullscreen" | "fullscreen" => Ok(IpcCommand::ToggleFullscreen),
        "list-layouts" | "list_layouts" | "layouts" => Ok(IpcCommand::ListLayouts),
        "list-plugins" | "list_plugins" | "plugins" => Ok(IpcCommand::ListPlugins),
        "reload-plugins" | "reload_plugins" | "reload" => Ok(IpcCommand::ReloadPlugins),
        "workspace" | "switch-workspace" | "switch_workspace" => {
            if args.len() != 2 {
                anyhow::bail!("usage: dinatorctl workspace <1-9>");
            }
            let workspace: usize = args[1].parse().context("invalid workspace number")?;
            Ok(IpcCommand::SwitchWorkspace { workspace })
        }
        "move-to-workspace" | "move_to_workspace" | "move-workspace" => {
            if args.len() != 2 {
                anyhow::bail!("usage: dinatorctl move-to-workspace <1-9>");
            }
            let workspace: usize = args[1].parse().context("invalid workspace number")?;
            Ok(IpcCommand::MoveToWorkspace { workspace })
        }
        "list-workspaces" | "list_workspaces" | "workspaces" => Ok(IpcCommand::ListWorkspaces),
        "gap" | "set-gap" | "set_gap" => {
            if args.len() != 2 {
                anyhow::bail!("usage: dinatorctl gap PIXELS");
            }
            let pixels: i32 = args[1].parse().context("invalid gap size")?;
            Ok(IpcCommand::SetGap { pixels })
        }
        "background" | "set-background" | "set_background" | "bg" => {
            if args.len() != 2 {
                anyhow::bail!("usage: dinatorctl background SPEC\n  SPEC: #RRGGBB, r,g,b (0-255 or 0.0-1.0)\n  gradient: COLOR-COLOR (e.g. #000000-#0000ff)");
            }
            Ok(IpcCommand::SetBackground { spec: args[1].clone() })
        }
        "output" => {
            if args.len() < 2 {
                anyhow::bail!("usage: dinatorctl output <create|remove|list|focus|move-to> [args...]");
            }
            match args[1].as_str() {
                "create" => {
                    if args.len() != 4 {
                        anyhow::bail!("usage: dinatorctl output create NAME WIDTHxHEIGHT");
                    }
                    let name = args[2].clone();
                    let parts: Vec<&str> = args[3].split('x').collect();
                    if parts.len() != 2 {
                        anyhow::bail!("resolution must be WIDTHxHEIGHT (e.g. 1920x1080)");
                    }
                    let width: u16 = parts[0].parse().context("invalid width")?;
                    let height: u16 = parts[1].parse().context("invalid height")?;
                    Ok(IpcCommand::CreateOutput { name, width, height })
                }
                "remove" | "rm" => {
                    if args.len() != 3 {
                        anyhow::bail!("usage: dinatorctl output remove NAME");
                    }
                    Ok(IpcCommand::RemoveOutput { name: args[2].clone() })
                }
                "list" | "ls" => Ok(IpcCommand::ListOutputs),
                "focus" => {
                    if args.len() != 3 {
                        anyhow::bail!("usage: dinatorctl output focus NAME");
                    }
                    Ok(IpcCommand::FocusOutput { name: args[2].clone() })
                }
                "move-to" | "move" => {
                    if args.len() != 3 {
                        anyhow::bail!("usage: dinatorctl output move-to NAME");
                    }
                    Ok(IpcCommand::MoveWindowToOutput { name: args[2].clone() })
                }
                sub => anyhow::bail!("unknown output subcommand: {sub}\n\nSubcommands: create, remove, list, focus, move-to"),
            }
        }
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
    master-grow          Grow master area (Alt+L)
    master-shrink        Shrink master area (Alt+H)
    layout NAME          Set tiling layout (column, monocle)
    float                Toggle focused window floating/tiled
    fullscreen           Toggle focused window fullscreen
    spawn CMD [ARGS]     Launch a program
    list-windows         List all managed windows
    layouts              List available layouts (built-in + plugins)
    plugins              List loaded plugins
    reload               Reload plugins from disk
    workspace N          Switch to workspace N (1-9)
    move-to-workspace N  Move focused window to workspace N
    workspaces           List all workspaces with window counts
    subscribe            Stream compositor events (JSON lines)
    gap PIXELS           Set gap/gutter between windows (e.g. 0, 4, 10)
    background SPEC      Set background (#RRGGBB, r,g,b, or gradient COLOR-COLOR)
    output create NAME WxH  Create a new headless output
    output remove NAME      Remove a headless output
    output list             List all outputs
    output focus NAME       Focus an output (receives keyboard input)
    output move-to NAME     Move focused window to an output
    quit                 Quit the compositor"
    );
}
