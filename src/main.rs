use std::collections::HashMap;
use std::io::Result;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, Mutex};
use uuid::Uuid;

#[derive(Debug)]
struct User {
    id: Uuid,
    nickname: String,
    whisper_sender: mpsc::Sender<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Setup listener for socket, broadcast channel for "All chat" and user list
    let listener = TcpListener::bind("127.0.0.1:8563").await?;
    let (tx, _rx) = broadcast::channel(10);
    let users = Arc::new(Mutex::new(HashMap::new()));
    let msg_history = Arc::new(Mutex::new(vec![("Server:".to_string(), "messages history".to_string())]));

    // Spawn task (in background) handling connections
    // Cloning sender broadcast channel and Arc user map for every connection (user)
    loop {
        let (socket, _) = listener.accept().await?;
        let tx = tx.clone();
        let users = Arc::clone(&users);
        let msg_history = Arc::clone(&msg_history);

        tokio::spawn(async move {
            handle_connection(socket, tx, users, msg_history).await;
        });
    }
}

async fn handle_connection(
    socket: TcpStream,
    tx: broadcast::Sender<(Uuid, String, String)>,
    users: Arc<Mutex<HashMap<Uuid, User>>>,
    msg_history: Arc<Mutex<Vec<(String, String)>>>,
) {
    // Split the socket : Reading part get inputs (Line we write), Writing part write what we get (Line from all chat / whisper ...)
    // In terminal : reader = what we write, writer = what can write on terminal
    let (reader, mut writer) = tokio::io::split(socket);
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();

    // Setup nickname and user map
    writer.write_all(b"Enter your nickname: ").await.unwrap();
    reader.read_line(&mut line).await.unwrap();
    let nickname = line.trim().to_string();
    line.clear();

    let user_id = Uuid::new_v4();

    // !!! Channels setup !!!
    // Whisper channel : MPSC (sender part) we give to the user struct so that we can send a msg to that user only
    // Write channel : MPSC to write everything else
    let (whisper_tx, whisper_rx) = mpsc::channel(10);
    let (write_tx, write_rx) = mpsc::channel(10);

    if let Err(err) = writer.write_all(format!("Welcome, {}!\n", nickname).as_bytes()).await {
        println!("Error sending welcome message to {}: {}", nickname, err);
    }

    // In own scop because of the Send trait error in task spawning if not
    {
        let mut users = users.lock().await;
        users.insert(
            user_id,
            User {
                id: user_id,
                nickname: nickname.clone(),
                whisper_sender: whisper_tx,
            },
        );

        let msg_history = msg_history.lock().await;
        for (nick, msg) in msg_history.iter() {
            let message = format!("{}: {}\n", nick, msg);
            writer.write_all(message.as_bytes()).await.unwrap();
        }
    };

    // Write Handle : Only focus to write to the socket (terminal)
    // Read Handle : Logic for sending and receving messages
    // Spawned in background but join so that we can quit
    let write_handle = tokio::spawn(handle_writes(writer, write_rx));
    let read_handle = tokio::spawn(handle_reads(
        reader,
        tx,
        user_id,
        nickname,
        users,
        write_tx,
        whisper_rx,
        msg_history,
    ));

    tokio::try_join!(write_handle, read_handle).unwrap();
}

async fn handle_writes(
    mut writer: tokio::io::WriteHalf<TcpStream>,
    mut write_rx: mpsc::Receiver<String>,
) {
    while let Some(message) = write_rx.recv().await {
        writer.write_all(message.as_bytes()).await.unwrap();
    }
}

async fn handle_reads(
    mut reader: tokio::io::BufReader<tokio::io::ReadHalf<TcpStream>>,
    tx: broadcast::Sender<(Uuid, String, String)>,
    user_id: Uuid,
    nickname: String,
    users: Arc<Mutex<HashMap<Uuid, User>>>,
    write_tx: mpsc::Sender<String>,
    mut whisper_rx: mpsc::Receiver<String>,
    msg_history: Arc<Mutex<Vec<(String, String)>>>,
) {
    let mut line = String::new();

    // Subscribe this connection to the broadcast channel so that we can receive "All messages" and write them
    // whisper_rx in parameter so that we can handle whispers receive
    let mut rx = tx.subscribe();

    // Use write_tx to only write for the user
    // Use tx to write in all chat
    loop {
        tokio::select! {
            result = reader.read_line(&mut line) => {
                if result.unwrap() == 0 {
                    break;
                }

                let msg = line.trim().to_string();

                if msg.starts_with("/whisper") {
                    let parts: Vec<&str> = msg.splitn(3, ' ').collect();
                    if parts.len() == 3 {
                        let target_nickname = parts[1];
                        let message = parts[2];
                        send_whisper(&nickname, target_nickname, message, &users, write_tx.clone()).await;
                    } else {
                        write_tx.send("Usage: /whisper <nickname> <message>\n".to_string()).await.unwrap();
                    }
                } else if msg.starts_with("/users") {
                    let mut users_list = String::new();
                    for (id, user) in users.lock().await.iter() {
                        users_list.push_str(&format!("{}: {}\n", id, user.nickname));
                    }
                    write_tx.send(users_list).await.unwrap();
                }
                else if msg.starts_with("/quit") {
                    break;
                }
                else if msg.starts_with("/help") {
                    write_tx.send("Available commands: /users, /whisper <nickname> <message>, /quit\n".to_string()).await.unwrap();
                } else {
                    // Broadcast (tx) used here
                    tx.send((user_id, nickname.clone(), msg.clone())).unwrap();
                    // History msg update
                    {
                        let mut history = msg_history.lock().await;
                        history.push((nickname.clone(), msg));
                    }
                }
                line.clear();
            }

            result = rx.recv() => {
                if let Ok((sender_id, sender_nickname, msg)) = result {
                    if sender_id != user_id {
                        write_tx.send(format!("{}: {}\n", sender_nickname, msg)).await.unwrap();
                    }
                }
            }
            result = whisper_rx.recv() => {
                if let Some(whisper) = result {
                    write_tx.send(format!("(Whisper) {}\n", whisper)).await.unwrap();
                }
            }
        }
    }

    {
        let mut users = users.lock().await;
        users.remove(&user_id);
    }
    
}

async fn send_whisper(
    sender_nickname: &str,
    target_nickname: &str,
    message: &str,
    users: &Arc<Mutex<HashMap<Uuid, User>>>,
    write_tx: mpsc::Sender<String>,
) {
    let users = users.lock().await;
    if let Some(target_user) = users.values().find(|user| user.nickname == target_nickname) {
        let private_message = format!("{}: {}", sender_nickname, message);
        target_user
            .whisper_sender
            .send(private_message)
            .await
            .unwrap();
    } else {
        write_tx
            .send(format!("User {} not found\n", target_nickname))
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncReadExt;
    use super::*;

    #[tokio::test]
    async fn test_message_history() {
        let history = Arc::new(Mutex::new(vec![
            ("user1".to_string(), "Hello".to_string()),
            ("user2".to_string(), "Hi".to_string()),
        ]));
        let (tx, _rx) = broadcast::channel(10);
        let users = Arc::new(Mutex::new(HashMap::new()));
    
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
    
        let history_clone = Arc::clone(&history);
        let users_clone = Arc::clone(&users);
        let tx_clone = tx.clone();
    
        let server_handle = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            handle_connection(socket, tx_clone, users_clone, history_clone).await;
        });
    
        let client_handle = tokio::spawn(async move {
            let mut socket = tokio::net::TcpStream::connect(addr).await.unwrap();
            socket.write_all(b"userTest\n").await.unwrap();
    
            let mut buffer = [0; 1024];
            let n = socket.read(&mut buffer).await.unwrap();
            let response = String::from_utf8_lossy(&buffer[..n]);
            assert!(response.contains("user1: Hello"));
            assert!(response.contains("user2: Hi"));
            assert!(!response.contains("user1: Test"));

            socket.write_all(b"yo").await.unwrap();
        });
    
        tokio::try_join!(server_handle, client_handle).unwrap();
    }
}
