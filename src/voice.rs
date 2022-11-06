use std::{
    fmt::Debug,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{SinkExt, StreamExt};
use opus::{Application, Channels, Decoder, Encoder};
use serde_json::{json, Value};
use tokio::{
    net::UdpSocket,
    sync::{
        oneshot::{self, Sender},
        Mutex,
    },
    time,
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use xsalsa20poly1305::{
    aead::{generic_array::GenericArray, Aead},
    KeyInit, XSalsa20Poly1305,
};

use crate::{
    audio_io::{AudioInput, AudioOutput},
    gateway::{CallInformation, WSSink, WSStream},
    Error,
};

#[derive(Debug, Clone)]
enum SendingState {
    NotSending,
    Sending,
    SilenceFrame(u8),
}

#[derive(Debug, Clone)]
pub struct Voice {
    write: Arc<Mutex<WSSink>>,
    socket: Arc<UdpSocket>,
    ssrc: u32,
    secret_key: Arc<Mutex<Option<[u8; 32]>>>,
    encoder: Arc<Mutex<Encoder>>,
    decoder: Arc<Mutex<Decoder>>,
    audio_output: Arc<Mutex<AudioOutput>>,
    audio_input: Arc<Mutex<AudioInput>>,
    sending_state: Arc<Mutex<SendingState>>,
}

impl Voice {
    pub async fn new(call_information: CallInformation) -> Result<Self, Error> {
        // Connect to websocket
        let (ws_stream, _) = connect_async(call_information.endpoint).await?;
        let (mut write, read) = ws_stream.split();
        info!("Connected to voice");

        // Identify
        let identify_payload = json!({
            "op": 0,
            "d": {
                "server_id": call_information.server_id,
                "user_id": call_information.user_id,
                "session_id": call_information.session_id,
                "token": call_information.token
            }
        })
        .to_string();
        write.send(Message::Text(identify_payload)).await.unwrap();

        let (ready_sender, ready_receiver) = oneshot::channel();

        let write_arc = Arc::new(Mutex::new(write));
        let ready_sender_arc = Arc::new(Mutex::new(Some(ready_sender)));

        let secret_key = Arc::new(Mutex::new(None));

        Self::read(
            write_arc.clone(),
            read,
            ready_sender_arc,
            secret_key.clone(),
        );

        // Wait for websocket to be ready
        let (ip, port, ssrc) = ready_receiver.await.map_err(|_| Error::ApplicationError)?;

        // Connect to udp socket
        let socket = UdpSocket::bind("0.0.0.0:3211")
            .await
            .map_err(|_| Error::SocketError)?;

        socket
            .connect(format!("{ip}:{port}"))
            .await
            .map_err(|_| Error::SocketError)?;

        // Send ip discovery packet
        let mut buf = [0_u8; 74];
        (&mut buf[0..]).write_u16::<BigEndian>(1).unwrap();
        (&mut buf[2..]).write_u16::<BigEndian>(70).unwrap();
        (&mut buf[4..]).write_u32::<BigEndian>(ssrc).unwrap();
        (&mut buf[72..]).write_u16::<BigEndian>(port).unwrap();

        socket.send(&buf).await.map_err(|_| Error::SocketError)?;

        let _self = Self {
            write: write_arc,
            socket: Arc::new(socket),
            ssrc: ssrc,
            secret_key,
            encoder: Arc::new(Mutex::new(
                Encoder::new(48000, Channels::Stereo, Application::Voip)
                    .map_err(|_| Error::ApplicationError)?,
            )),
            decoder: Arc::new(Mutex::new(
                Decoder::new(48000, Channels::Stereo).map_err(|_| Error::ApplicationError)?,
            )),
            audio_output: Arc::new(Mutex::new(AudioOutput::new())),
            audio_input: Arc::new(Mutex::new(AudioInput::new())),
            sending_state: Arc::new(Mutex::new(SendingState::NotSending)),
        };

        _self.read_socket();
        _self.send_audio();

        Ok(_self)
    }

    fn read(
        write: Arc<Mutex<WSSink>>,
        read: WSStream,
        ready_sender: Arc<Mutex<Option<Sender<(String, u16, u32)>>>>,
        secret_key: Arc<Mutex<Option<[u8; 32]>>>,
    ) {
        tokio::spawn(async move {
            read.for_each(|message| async {
                let str = message.unwrap().into_text().unwrap();
                let msg: Value = match serde_json::from_str(&str) {
                    Ok(m) => m,
                    Err(_) => panic!("WebSocket closed: {}", str),
                };

                match msg["op"].as_u64().unwrap() {
                    2 => {
                        if let Some(sender) = ready_sender.lock().await.take() {
                            let d = &msg["d"];
                            sender
                                .send((
                                    d["ip"].as_str().unwrap().to_owned(),
                                    d["port"].as_u64().unwrap() as u16,
                                    d["ssrc"].as_u64().unwrap() as u32,
                                ))
                                .unwrap();
                        }
                    }
                    4 => {
                        let mut key = [0; 32];
                        msg["d"]["secret_key"]
                            .as_array()
                            .unwrap()
                            .into_iter()
                            .enumerate()
                            .for_each(|(i, b)| key[i] = b.as_u64().unwrap() as u8);

                        let _ = secret_key.lock().await.insert(key);
                    }
                    8 => {
                        let heartbeat_interval = msg["d"]["heartbeat_interval"].as_u64().unwrap();

                        let write2 = write.clone();
                        tokio::spawn(async move {
                            let mut interval =
                                time::interval(Duration::from_millis(heartbeat_interval));
                            interval.tick().await;

                            loop {
                                interval.tick().await;

                                let seconds = SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap()
                                    .as_secs();

                                let heartbeat_payload = json!({
                                    "op": 3,
                                    "d": seconds
                                })
                                .to_string();
                                write2
                                    .lock()
                                    .await
                                    .send(Message::Text(heartbeat_payload))
                                    .await
                                    .unwrap();
                            }
                        });
                    }
                    6 => {}
                    op => {
                        warn!("voice => Received unhandled op code {op}: {msg}");
                    }
                }
            })
            .await;
        });
    }

    fn read_socket(&self) {
        let write = self.write.clone();
        let socket = self.socket.clone();
        let secret_key = self.secret_key.clone();
        let decoder = self.decoder.clone();
        let audio_output = self.audio_output.clone();
        tokio::spawn(async move {
            let mut msg_buffer = [0_u8; 1024];
            let mut pcm_buffer = [0_i16; 1920];
            let mut nonce_buffer = [0_u8; 24];
            loop {
                let len = socket.recv(&mut msg_buffer).await.unwrap();

                match msg_buffer[1] {
                    0x02 => {
                        // Ip discovery answer
                        let ip_bytes = msg_buffer[8..72]
                            .into_iter()
                            .cloned()
                            .filter(|b| *b != 0)
                            .collect::<Vec<_>>();
                        let ip = std::str::from_utf8(&ip_bytes[..]).unwrap();

                        let port = (&msg_buffer[72..]).read_u16::<BigEndian>().unwrap();

                        let protocol_payload = json!({
                            "op": 1,
                            "d": {
                                "protocol": "udp",
                                "data": {
                                    "address": ip,
                                    "port": port,
                                    "mode": "xsalsa20_poly1305"
                                }
                            }
                        })
                        .to_string();
                        write
                            .lock()
                            .await
                            .send(Message::Text(protocol_payload))
                            .await
                            .unwrap();
                    }
                    0x78 => {
                        // Voice data
                        if let Some(secret_key) = secret_key.lock().await.as_ref() {
                            let _ = &nonce_buffer[..12].copy_from_slice(&msg_buffer[..12]);

                            let decrypted =
                                XSalsa20Poly1305::new(GenericArray::from_slice(secret_key))
                                    .decrypt(
                                        GenericArray::from_slice(&nonce_buffer),
                                        &msg_buffer[12..len],
                                    )
                                    .unwrap();

                            // Skip the first 8 extension bytes if the extension bit is set
                            let start = if msg_buffer[0] & 0b00010000 > 0 { 8 } else { 0 };

                            let _ = decoder
                                .lock()
                                .await
                                .decode(&decrypted[start..], &mut pcm_buffer, false)
                                .unwrap();

                            audio_output.lock().await.push(&pcm_buffer);
                        }
                    }
                    _ => info!("{len:?} bytes received: {:?}", &msg_buffer[..len]),
                }
            }
        });
    }

    fn send_audio(&self) {
        let socket = self.socket.clone();
        let secret_key = self.secret_key.clone();
        let encoder = self.encoder.clone();
        let audio_input = self.audio_input.clone();
        let sending_state = self.sending_state.clone();
        let ssrc = self.ssrc.clone();
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_millis(20));

            let silence: [u8; 3] = [0xF8, 0xFF, 0xFE];

            let mut pcm_mono_buffer = [0_i16; 960];
            let mut pcm_stereo_buffer = [0_i16; 1920];
            let mut opus_buffer = [0_u8; 1024];
            let mut msg_buffer = [0_u8; 1024];
            let mut nonce_buffer = [0_u8; 24];

            let mut sequence = 0_u16;
            let mut timestamp = 0_u32;

            // RTP header
            msg_buffer[0] = 0x80;
            msg_buffer[1] = 0x78;
            (&mut msg_buffer[2..])
                .write_u16::<BigEndian>(sequence)
                .unwrap();
            (&mut msg_buffer[4..])
                .write_u32::<BigEndian>(timestamp)
                .unwrap();
            (&mut msg_buffer[8..]).write_u32::<BigEndian>(ssrc).unwrap();

            loop {
                interval.tick().await;

                audio_input.lock().await.pop(&mut pcm_mono_buffer);

                let mut state = sending_state.lock().await;
                match *state {
                    SendingState::Sending => {
                        if let Some(secret_key) = secret_key.lock().await.as_ref() {
                            // Dulicate each sample to get stereo audio
                            for (i, b) in pcm_mono_buffer.into_iter().enumerate() {
                                pcm_stereo_buffer[i * 2] = b;
                                pcm_stereo_buffer[i * 2 + 1] = b;
                            }

                            let len = encoder
                                .lock()
                                .await
                                .encode(&pcm_stereo_buffer, &mut opus_buffer)
                                .unwrap();

                            let _ = &nonce_buffer[..12].copy_from_slice(&msg_buffer[..12]);

                            let encrypted =
                                XSalsa20Poly1305::new(GenericArray::from_slice(secret_key))
                                    .encrypt(
                                        GenericArray::from_slice(&nonce_buffer),
                                        &opus_buffer[..len],
                                    )
                                    .unwrap();

                            let _ = &msg_buffer[12..12 + encrypted.len()]
                                .copy_from_slice(encrypted.as_slice());

                            socket
                                .send(&msg_buffer[..12 + encrypted.len()])
                                .await
                                .unwrap();

                            sequence = sequence.wrapping_add(1);
                            timestamp = timestamp.wrapping_add(960);

                            (&mut msg_buffer[2..])
                                .write_u16::<BigEndian>(sequence)
                                .unwrap();
                            (&mut msg_buffer[4..])
                                .write_u32::<BigEndian>(timestamp)
                                .unwrap();
                        }
                    }
                    SendingState::SilenceFrame(i) => {
                        *state = if i < 5 {
                            SendingState::SilenceFrame(i + 1)
                        } else {
                            SendingState::NotSending
                        };

                        if let Some(secret_key) = secret_key.lock().await.as_ref() {
                            let _ = &nonce_buffer[..12].copy_from_slice(&msg_buffer[..12]);

                            let encrypted =
                                XSalsa20Poly1305::new(GenericArray::from_slice(secret_key))
                                    .encrypt(GenericArray::from_slice(&nonce_buffer), &silence[..])
                                    .unwrap();

                            let _ = &msg_buffer[12..12 + encrypted.len()]
                                .copy_from_slice(encrypted.as_slice());

                            socket
                                .send(&msg_buffer[..12 + encrypted.len()])
                                .await
                                .unwrap();

                            sequence = sequence.wrapping_add(1);
                            timestamp = timestamp.wrapping_add(960);

                            (&mut msg_buffer[2..])
                                .write_u16::<BigEndian>(sequence)
                                .unwrap();
                            (&mut msg_buffer[4..])
                                .write_u32::<BigEndian>(timestamp)
                                .unwrap();
                        }
                    }
                    SendingState::NotSending => {}
                }
            }
        });
    }

    pub async fn set_sending(self: Arc<Self>, sending: bool) {
        let speaking_payload = json!({
            "op": 5,
            "d": {
                "speaking": if sending { 1 } else { 0 },
                "delay": 5,
                "ssrc": self.ssrc
            }
        })
        .to_string();

        self.write
            .lock()
            .await
            .send(Message::Text(speaking_payload))
            .await
            .unwrap();

        *self.sending_state.lock().await = if sending {
            SendingState::Sending
        } else {
            SendingState::SilenceFrame(0)
        }
    }
}
