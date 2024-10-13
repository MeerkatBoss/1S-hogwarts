use std::collections::{HashMap, VecDeque};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, FramedWrite, LinesCodec};
use serde::{Deserialize, Serialize};
use futures::SinkExt;

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum GuessResult {
    Correct,
    Higher,
    Lower,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "message", content = "content")]
pub enum ServerMessage {
    ExperimentStarted,
    ExperimentEnded,
    GuessEvaluated { result: GuessResult },
    CustomMessage(String),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ClientMessage {
    Guess(u64),
    RequestHistory,
}

#[derive(Debug, Clone)]
pub struct Experiment {
    attempts: HashMap<u64, Vec<u64>>,
}

impl Experiment {
    fn new() -> Self {
        Experiment {
            attempts: HashMap::new(),
        }
    }
}

pub struct Server {
    client_counter: AtomicU64,
    clients: AsyncMutex<HashMap<u64, mpsc::UnboundedSender<ServerMessage>>>,
    pending_responses: AsyncMutex<VecDeque<(u64, u64)>>,
    experiments: AsyncMutex<Vec<Experiment>>,
}

impl Server {
    fn new() -> Server {
        Server {
            clients: AsyncMutex::new(HashMap::new()),
            pending_responses: AsyncMutex::new(VecDeque::new()),
            client_counter: AtomicU64::new(0),
            experiments: AsyncMutex::new(Vec::new()),
        }
    }

    async fn listen(self: Arc<Self>, address: &str) {
        let listener = TcpListener::bind(address).await.unwrap();
        println!("Server is listening on {}", address);

        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    let client_id = self.client_counter.fetch_add(1, Ordering::Relaxed) + 1;
                    println!("New client connected: Client ID {}", client_id);

                    let server_clone = self.clone();
                    tokio::spawn(async move {
                        server_clone.handle_client(stream, client_id).await;
                    });
                }
                Err(e) => {
                    eprintln!("Failed to accept connection: {}", e);
                }
            }
        }
    }

    async fn handle_client(self: Arc<Self>, stream: TcpStream, client_id: u64) {
        let (reader, writer) = stream.into_split();

        let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();

        {
            let mut clients = self.clients.lock().await;
            clients.insert(client_id, tx);
        }

        let self_clone = self.clone();

        let read_task = tokio::spawn(async move {
            let mut reader = FramedRead::new(reader, LinesCodec::new());
            while let Some(result) = reader.next().await {
                match result {
                    Ok(line) => {
                        let message = match serde_json::from_str::<ClientMessage>(&line) {
                            Ok(msg) => msg,
                            Err(e) => {
                                eprintln!(
                                    "Failed to parse message from client {}: {}",
                                    client_id, e
                                );
                                continue;
                            }
                        };
                        println!(
                            "Received message from Client ID {}: {:?}",
                            client_id, message
                        );
                        match message {
                            ClientMessage::Guess(guess) => {
                                let mut pending_responses =
                                    self_clone.pending_responses.lock().await;
                                pending_responses.push_back((client_id, guess));
                            }
                            ClientMessage::RequestHistory => {}
                        }
                    }
                    Err(e) => {
                        eprintln!("Failed to read from client {}: {}", client_id, e);
                        break;
                    }
                }
            }
            println!("Client disconnected: Client ID {}", client_id);
            self_clone.clients.lock().await.remove(&client_id);
        });

        let write_task = tokio::spawn(async move {
            let mut writer = FramedWrite::new(writer, LinesCodec::new());
            while let Some(message) = rx.recv().await {
                let message_str = serde_json::to_string(&message).unwrap();
                if let Err(e) = writer.send(message_str).await {
                    eprintln!("Failed to send message to client {}: {}", client_id, e);
                    break;
                }
            }
        });

        let _ = tokio::join!(read_task, write_task);
    }

    async fn send_to_client(&self, client_id: u64, message: &ServerMessage) {
        let clients = self.clients.lock().await;
        if let Some(tx) = clients.get(&client_id) {
            if let Err(e) = tx.send(message.clone()) {
                eprintln!("Failed to send message to client {}: {}", client_id, e);
            }
        } else {
            eprintln!("Client {} not found", client_id);
        }
    }

    async fn start_experiment(&self) {
        let mut experiments = self.experiments.lock().await;
        experiments.push(Experiment::new());

        let clients = self.clients.lock().await;
        for client_id in clients.keys() {
            println!("Notifying Client ID {} about experiment start", client_id);
            self.send_to_client(*client_id, &ServerMessage::ExperimentStarted)
                .await;
        }
    }

    async fn respond_to_guesses(&self) {
        loop {
            let pending_responses_len = {
                let pending_responses = self.pending_responses.lock().await;
                pending_responses.len()
            };

            if pending_responses_len == 0 {
                println!("No pending responses.");
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                continue;
            }

            self.view_pending_guesses().await;

            println!("Enter the number of the guess you want to respond to (or 'q' to quit):");
            let input = self.read_line().await.trim().to_string();

            if input == "q" {
                break;
            }

            let index: usize = match input.parse::<usize>() {
                Ok(num) => num - 1,
                Err(_) => {
                    println!("Invalid input. Please try again.");
                    continue;
                }
            };

            let (client_id, guess) = {
                let pending_responses = self.pending_responses.lock().await;
                if let Some((client_id, guess)) = pending_responses.get(index) {
                    (*client_id, *guess)
                } else {
                    println!("Invalid index. Please try again.");
                    continue;
                }
            };

            println!(
                "Enter response for guess {} from Client ID {} (correct, higher, lower): ",
                guess, client_id
            );
            let response = self.read_line().await.trim().to_string();

            let result = match response.as_str() {
                "correct" => GuessResult::Correct,
                "higher" => GuessResult::Higher,
                "lower" => GuessResult::Lower,
                _ => {
                    println!("Invalid response. Try again.");
                    continue;
                }
            };

            {
                let mut experiments = self.experiments.lock().await;
                if let Some(current_experiment) = experiments.last_mut() {
                    current_experiment
                        .attempts
                        .entry(client_id)
                        .or_insert_with(Vec::new)
                        .push(guess);
                } else {
                    println!("No active experiment. Start a new experiment first.");
                    break;
                }
            }

            let response_message = ServerMessage::GuessEvaluated { result };

            self.send_to_client(client_id, &response_message).await;

            {
                let mut pending_responses = self.pending_responses.lock().await;
                pending_responses.remove(index);
            }

            if let GuessResult::Correct = result {
                println!("Client ID {} has guessed the number!", client_id);
            }
        }
    }

    async fn send_message_to_all(&self) {
        println!("Enter the message to send to all clients:");
        let message_text = self.read_line().await.trim().to_string();
        let server_message = ServerMessage::CustomMessage(message_text);

        let clients = self.clients.lock().await;
        for client_id in clients.keys() {
            self.send_to_client(*client_id, &server_message).await;
        }
        println!("Message sent to all clients.");
    }

    async fn handle_scientist_input(self: Arc<Self>) {
        loop {
            println!("1. View pending guesses");
            println!("2. Respond to a pending guess");
            println!("3. Start experiment");
            println!("4. View leaderboard");
            println!("5. Send message to all clients");
            let choice = self.read_line().await;

            match choice.trim() {
                "1" => {
                    self.view_pending_guesses().await;
                }
                "2" => {
                    self.respond_to_guesses().await;
                }
                "3" => {
                    self.start_experiment().await;
                }
                "4" => {
                    self.view_leaderboard().await;
                }
                "5" => {
                    self.send_message_to_all().await;
                }
                _ => {
                    println!("Invalid choice, try again");
                }
            }
        }
    }

    async fn read_line(&self) -> String {
        tokio::task::spawn_blocking(|| {
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).unwrap();
            input
        })
        .await
        .unwrap()
    }

    async fn view_pending_guesses(&self) {
        let pending_responses = self.pending_responses.lock().await;
        if pending_responses.is_empty() {
            println!("No pending responses.");
        } else {
            println!("Pending guesses:");
            for (i, (client_id, guess)) in pending_responses.iter().enumerate() {
                println!("{}. Client ID: {}, Guess: {}", i + 1, client_id, guess);
            }
        }
    }

    async fn view_leaderboard(&self) {
        let experiments = self.experiments.lock().await;
        println!("Leaderboard:");

        let mut leaderboard: HashMap<u64, usize> = HashMap::new();

        for experiment in experiments.iter() {
            for (client_id, attempts) in experiment.attempts.iter() {
                let entry = leaderboard.entry(*client_id).or_insert(0);
                *entry += attempts.len();
            }
        }

        let mut leaderboard: Vec<(u64, usize)> = leaderboard.into_iter().collect();
        leaderboard.sort_by_key(|&(_, attempts)| attempts);

        for (client_id, attempts) in leaderboard {
            println!("Client ID: {}, Total Attempts: {}", client_id, attempts);
        }
    }
}

async fn client_mode() {
    println!("Enter server address (e.g., 127.0.0.1:7878):");
    let server_addr = read_line().await.trim().to_string();

    match TcpStream::connect(server_addr).await {
        Ok(stream) => {
            println!("Connected to the server.");
            let (reader, writer) = stream.into_split();

            let (tx, mut rx) = mpsc::unbounded_channel::<ClientMessage>();

            let guess_history = Arc::new(AsyncMutex::new(Vec::new()));

            let guess_history_clone = Arc::clone(&guess_history);

            let reader_task = tokio::spawn(async move {
                let mut reader = FramedRead::new(reader, LinesCodec::new());
                while let Some(result) = reader.next().await {
                    match result {
                        Ok(line) => {
                            let message = match serde_json::from_str::<ServerMessage>(&line) {
                                Ok(msg) => msg,
                                Err(e) => {
                                    eprintln!("Failed to parse message from server: {}", e);
                                    continue;
                                }
                            };
                            handle_server_message(&message, &guess_history_clone).await;
                        }
                        Err(e) => {
                            eprintln!("Failed to read from server: {}", e);
                            break;
                        }
                    }
                }
            });

            let writer_task = tokio::spawn(async move {
                let mut writer = FramedWrite::new(writer, LinesCodec::new());
                while let Some(message) = rx.recv().await {
                    let message_str = serde_json::to_string(&message).unwrap();
                    if let Err(e) = writer.send(message_str).await {
                        eprintln!("Failed to send message to server: {}", e);
                        break;
                    }
                }
            });

            loop {
                println!("Enter your guess, 'history' to view your guesses, or 'wait' to wait for messages:");
                let input = read_line().await.trim().to_string();

                if input.to_lowercase() == "history" {
                    let history = guess_history.lock().await;
                    if history.is_empty() {
                        println!("You have not made any guesses yet.");
                    } else {
                        println!("Your guess history:");
                        for (i, guess) in history.iter().enumerate() {
                            println!("{}. {}", i + 1, guess);
                        }
                    }
                    continue;
                } else if input.to_lowercase() == "wait" {
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                    continue;
                }

                match input.parse::<u64>() {
                    Ok(guess) => {
                        {
                            let mut history = guess_history.lock().await;
                            history.push(guess);
                        }
                        let message = ClientMessage::Guess(guess);
                        if tx.send(message).is_err() {
                            eprintln!("Failed to send message to server.");
                            break;
                        }
                    }
                    Err(_) => {
                        println!("Invalid input. Please enter a number.");
                    }
                }
            }

            let _ = tokio::join!(reader_task, writer_task);
        }
        Err(e) => {
            eprintln!("Failed to connect to server: {}", e);
        }
    }
}

async fn handle_server_message(
    message: &ServerMessage,
    guess_history: &Arc<AsyncMutex<Vec<u64>>>,
) {
    match message {
        ServerMessage::ExperimentStarted => {
            println!("Experiment has started!");
            let mut history = guess_history.lock().await;
            history.clear();
        }
        ServerMessage::ExperimentEnded => {
            println!("Experiment has ended!");
        }
        ServerMessage::GuessEvaluated { result } => match result {
            GuessResult::Correct => {
                println!("Your guess was correct!");
            }
            GuessResult::Higher => {
                println!("Your guess was too low.");
            }
            GuessResult::Lower => {
                println!("Your guess was too high.");
            }
        },
        ServerMessage::CustomMessage(text) => {
            println!("Message from scientists: {}", text);
        }
    }
}

async fn read_line() -> String {
    tokio::task::spawn_blocking(|| {
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).unwrap();
        input
    })
    .await
    .unwrap()
}

#[tokio::main]
async fn main() {
    println!("Enter 'server' to run as server or 'client' to run as client:");
    let mut mode = String::new();
    std::io::stdin().read_line(&mut mode).unwrap();

    match mode.trim() {
        "server" => {
            let server = Arc::new(Server::new());

            let server_clone = server.clone();
            tokio::spawn(async move {
                server_clone.listen("127.0.0.1:7878").await;
            });

            server.handle_scientist_input().await;
        }
        "client" => {
            client_mode().await;
        }
        _ => {
            println!("Invalid mode, exiting");
        }
    }
}
