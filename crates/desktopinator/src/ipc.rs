use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex};

use dinator_ipc::{IpcCommand, IpcEvent, IpcResponse};
use smithay::reexports::calloop::channel;
use tracing::info;

/// An IPC command paired with a response channel back to the client.
pub struct IpcRequest {
    pub command: IpcCommand,
    pub respond: Box<dyn FnOnce(IpcResponse) + Send>,
}

/// Subscribers: IPC client threads register their sender here.
pub type EventSubscribers = Arc<Mutex<Vec<std::sync::mpsc::Sender<IpcEvent>>>>;

/// Start the IPC Unix socket listener in a background thread.
/// Returns a calloop channel receiver that delivers parsed IPC commands.
pub fn start_ipc_server(
    subscribers: EventSubscribers,
) -> anyhow::Result<channel::Channel<IpcRequest>> {
    let socket_path = dinator_ipc::socket_path();

    // Remove stale socket if it exists
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)?;
    }

    let listener = UnixListener::bind(&socket_path)?;
    listener.set_nonblocking(false)?;
    info!(path = %socket_path.display(), "IPC socket listening");

    let (tx, rx) = channel::channel::<IpcRequest>();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let tx = tx.clone();
                    let subscribers = subscribers.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = handle_client(stream, tx, subscribers) {
                            tracing::error!("IPC client error: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("IPC accept error: {e}");
                }
            }
        }
    });

    Ok(rx)
}

fn handle_client(
    stream: UnixStream,
    tx: channel::Sender<IpcRequest>,
    subscribers: EventSubscribers,
) -> anyhow::Result<()> {
    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    for line in reader.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let command: IpcCommand = match serde_json::from_str(line) {
            Ok(cmd) => cmd,
            Err(e) => {
                let resp = IpcResponse::Error {
                    message: format!("invalid command: {e}"),
                };
                let mut msg = serde_json::to_string(&resp)?;
                msg.push('\n');
                writer.write_all(msg.as_bytes())?;
                continue;
            }
        };

        // Handle Subscribe locally — switch to streaming mode
        if matches!(command, IpcCommand::Subscribe) {
            // Send OK to confirm subscription
            let resp = IpcResponse::Ok {
                message: Some("subscribed".to_string()),
            };
            let mut msg = serde_json::to_string(&resp)?;
            msg.push('\n');
            writer.write_all(msg.as_bytes())?;

            // Register this connection as a subscriber
            let (event_tx, event_rx) = std::sync::mpsc::channel::<IpcEvent>();
            subscribers.lock().unwrap().push(event_tx);

            // Stream events until the client disconnects or the channel closes
            for event in event_rx {
                let mut msg = serde_json::to_string(&event)?;
                msg.push('\n');
                if writer.write_all(msg.as_bytes()).is_err() {
                    break; // client disconnected
                }
            }
            return Ok(());
        }

        // Send command to the compositor event loop and wait for response
        let (resp_tx, resp_rx) = std::sync::mpsc::channel::<IpcResponse>();
        let request = IpcRequest {
            command,
            respond: Box::new(move |resp| {
                let _ = resp_tx.send(resp);
            }),
        };

        if tx.send(request).is_err() {
            break; // compositor shut down
        }

        // Wait for the compositor to process the command
        let response = resp_rx.recv().unwrap_or(IpcResponse::Error {
            message: "compositor did not respond".to_string(),
        });

        let mut msg = serde_json::to_string(&response)?;
        msg.push('\n');
        writer.write_all(msg.as_bytes())?;
    }

    Ok(())
}
