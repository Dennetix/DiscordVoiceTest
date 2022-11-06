use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use serde_json::{json, Value};
use tokio::{
    net::TcpStream,
    sync::{
        oneshot::{self, Sender},
        Mutex, Notify,
    },
    time,
};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};
use url::Url;

use crate::Error;

const URL: &str = "wss://gateway.discord.gg/?v=9";

pub type WSSink = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>;
pub type WSStream = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;

#[derive(Debug, Clone)]
pub struct CallInformation {
    pub endpoint: Url,
    pub server_id: String,
    pub user_id: String,
    pub session_id: String,
    pub token: String,
}

#[derive(Debug, Clone)]
pub struct Gateway {
    write: Arc<Mutex<WSSink>>,
    sequence: Arc<AtomicU32>,
    ready_notify: Arc<Notify>,
    voice_state_sender: Arc<Mutex<Option<Sender<(String, String)>>>>,
    voice_server_sender: Arc<Mutex<Option<Sender<(String, String)>>>>,
}

impl Gateway {
    pub async fn new() -> Result<Self, Error> {
        let url = url::Url::parse(URL).map_err(|_| Error::ApplicationError)?;

        let (ws_stream, _) = connect_async(url).await?;
        let (write, read) = ws_stream.split();
        info!("Connected to gateway");

        let _self = Self {
            write: Arc::new(Mutex::new(write)),
            sequence: Arc::new(AtomicU32::new(0)),
            ready_notify: Arc::new(Notify::new()),
            voice_state_sender: Arc::new(Mutex::new(None)),
            voice_server_sender: Arc::new(Mutex::new(None)),
        };

        _self.read(read);

        Ok(_self)
    }

    fn read(&self, read: WSStream) {
        let write = self.write.clone();
        let sequence = self.sequence.clone();
        let ready_notify = self.ready_notify.clone();
        let voice_state_sender = self.voice_state_sender.clone();
        let voice_server_sender = self.voice_server_sender.clone();
        tokio::spawn(async move {
            read.for_each(|message| async {
                let str = message.unwrap().into_text().unwrap();
                let msg: Value = match serde_json::from_str(&str) {
                    Ok(m) => m,
                    Err(_) => panic!("Gateway WebSocket closed: {}", str),
                };

                match msg["op"].as_u64().unwrap() {
                    0 => {
                        sequence.store(msg["s"].as_i64().unwrap() as u32, Ordering::SeqCst);

                        match msg["t"].as_str().unwrap() {
                            "READY" => ready_notify.notify_one(),
                            "VOICE_STATE_UPDATE" => {
                                if let Some(sender) = voice_state_sender.lock().await.take() {
                                    let d = &msg["d"];
                                    sender
                                        .send((
                                            d["session_id"].as_str().unwrap().to_owned(),
                                            d["user_id"].as_str().unwrap().to_owned(),
                                        ))
                                        .unwrap();
                                }
                            }
                            "VOICE_SERVER_UPDATE" => {
                                if let Some(sender) = voice_server_sender.lock().await.take() {
                                    let d = &msg["d"];
                                    sender
                                        .send((
                                            d["endpoint"].as_str().unwrap().to_owned(),
                                            d["token"].as_str().unwrap().to_owned(),
                                        ))
                                        .unwrap();
                                }
                            }
                            msg_type => warn!("Received unhandled dispatch of type {msg_type}"),
                        }
                    }
                    10 => {
                        let heartbeat_interval = msg["d"]["heartbeat_interval"].as_u64().unwrap();

                        let write2 = write.clone();
                        let sequence2 = sequence.clone();
                        tokio::spawn(async move {
                            let mut interval =
                                time::interval(Duration::from_millis(heartbeat_interval));
                            interval.tick().await;

                            loop {
                                interval.tick().await;

                                let s = sequence2.load(Ordering::SeqCst);
                                let heartbeat_payload = if s > 0 {
                                    json!({"op": 1, "d": s}).to_string()
                                } else {
                                    json!({"op": 1, "d": null}).to_string()
                                };
                                write2
                                    .lock()
                                    .await
                                    .send(Message::Text(heartbeat_payload))
                                    .await
                                    .unwrap();
                            }
                        });
                    }
                    11 => {}
                    op => {
                        warn!("=> Received unhandled op code {op}: {msg}");
                    }
                }
            })
            .await;
        });
    }

    pub async fn identify(self: Arc<Self>, token: String) -> Result<(), Error> {
        let indentify_payload = json!({
            "op": 2,
            "d": {
                "token": token,
                "properties": {
                    "capabilities": 1021,
                    "os": "Windows",
                    "browser": "Chrome",
                    "device": ""
                }
            }
        })
        .to_string();
        self.write
            .lock()
            .await
            .send(Message::Text(indentify_payload))
            .await?;

        self.ready_notify.notified().await;

        Ok(())
    }

    pub async fn start_call(self: Arc<Self>, channel_id: String) -> Result<CallInformation, Error> {
        let voice_payload = json!({
            "op": 4,
            "d": {
                "guild_id": null,
                "channel_id": channel_id,
                "self_mute": false,
                "self_deaf": false,
                "self_video": false
            }
        })
        .to_string();

        let (state_sender, state_receiver) = oneshot::channel::<(String, String)>();
        let (server_sender, server_receiver) = oneshot::channel::<(String, String)>();
        let _ = self.voice_state_sender.lock().await.insert(state_sender);
        let _ = self.voice_server_sender.lock().await.insert(server_sender);

        self.write
            .lock()
            .await
            .send(Message::Text(voice_payload))
            .await?;

        let (session_id, user_id) = state_receiver.await.map_err(|_| Error::ApplicationError)?;
        let (endpoint, token) = server_receiver.await.map_err(|_| Error::ApplicationError)?;

        Ok(CallInformation {
            endpoint: Url::parse(&format!("wss://{endpoint}/?v=7"))
                .map_err(|_| Error::ApplicationError)?,
            server_id: channel_id,
            user_id,
            session_id,
            token,
        })
    }
}
