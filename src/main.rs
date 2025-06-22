use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use axum::{
    routing::get,
    Router,
    extract::ws::{WebSocket, WebSocketUpgrade, Message},
    response::IntoResponse,
};
use tokio::process::{Command, ChildStdin, ChildStdout};
use tokio::io::{AsyncWriteExt, AsyncReadExt, BufReader};
use futures::{StreamExt, SinkExt};
use tokio::sync::mpsc;
use dashmap::DashMap;
use once_cell::sync::OnceCell;
use uuid::Uuid;

/// An atomic reference-counted, thread-safe DashMap, mapping u64 keys to channel senders for sending WebSocket messages.
type Users = Arc<DashMap<u128, mpsc::UnboundedSender<Message>>>;

/// Global USERS variable, initializable at runtime only once
static USERS: OnceCell<Users> = OnceCell::new();
// Initialize USERS on startup

#[tokio::main]
async fn main() {
    let users = Arc::new(DashMap::new());
    USERS.set(users).unwrap();

    let app = Router::new()
        .route("/ws", get(ws_shell_handler));

    let addr = SocketAddr::from(([0, 0, 0, 0], 5327)); // SERV = 5327
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap();
    println!("🚀 Rust server listening on http://localhost:5327");
    axum::serve(listener, app).await.unwrap();
}

async fn ws_shell_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    println!("In ws_shell_handler.");
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    println!("In handle_socket.");

    /// Given a socket, split it
    let (mut ws_sender, mut ws_receiver) = socket.split();
    /// Create an unbounded channel to FIFO any messages to or from client
    let (sender, mut receiver) = mpsc::unbounded_channel();
    /// Spawn a task that waits for receiver to get messages and sends them to ws
    let _outbound_fifo_task = tokio::spawn(async move {
        while let Some(msg) = receiver.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    /// Create a client_id for the connection
    let client_id = Uuid::new_v4().as_u128();
    /// Add to users
    let users = USERS.get().unwrap();
    // Make a clone for USERS first
    let sender_for_users = sender.clone();
    users.insert(client_id, sender_for_users);
    println!("Users inserted: {client_id}, new sender");

    // Spawn servshell instance in OS
    let mut child = match Command::new("./bin/servsh")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            eprintln!("Failed to launch shell: {e}");
            return;
        }
    };
    println!("Child process 'servsh' spawned.");

    /// Handle inbound ws starting ping msg and ending close message
    while let Some(Ok(msg)) = ws_receiver.next().await {
        println!("Received ws msg: {msg:?} from client id: {client_id}");

        match msg {
            Message::Text(text) => {
                /// Ping message is sent at the beginning connecting to ws
                if text == "ping" {
                    /// Send back pong immediately for latency calculation
                    if let Some(sender) = users.get(&client_id) {
                        let _ = sender.send(Message::Text("pong".into()));
                    }
                    println!("Ping from {client_id} received.");
                }
            },
            Message::Close(_) => {
                users.remove(&client_id);
                break;
            },
            _ => {}, // default pass
        }
    }

    let mut child_stdin = child.stdin.take().unwrap();
    let mut child_stdout = BufReader::new(child.stdout.take().unwrap());

    // Task: from WebSocket -> child stdin
    let ws_to_stdin = async {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            if let Message::Text(txt) = msg {
                // Write to child stdin
                if child_stdin.write_all(txt.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    };

    // Task: from child stdout -> sender (not receiver!!)
    let stdout_to_ws = {
        let sender = sender.clone(); // clone for this task
        async move {
            let mut buf = [0u8; 1024];
            loop {
                match child_stdout.read(&mut buf).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        match std::str::from_utf8(&buf[..n]) {
                            Ok(s) => {
                                // Use sender!
                                if sender.send(Message::Text(s.into())).is_err() {
                                    break;
                                }
                            }
                            Err(_) => {
                                // Optionally handle binary
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    };

    // Run both directions concurrently
    tokio::join!(ws_to_stdin, stdout_to_ws);
}
