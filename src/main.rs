use std::sync::Arc;

use gateway::{CallInformation, Gateway};
use iced::theme::Button;
use iced::widget::{button, column, container, row, text, text_input};
use iced::{executor, window, Application, Color, Command, Length, Settings, Theme};
use tracing::{error, Level};
use tracing_subscriber::FmtSubscriber;
use voice::Voice;

mod audio_io;
mod gateway;
mod voice;

#[tokio::main]
async fn main() -> iced::Result {
    // Logging
    tracing::subscriber::set_global_default(
        FmtSubscriber::builder()
            .with_max_level(Level::INFO)
            .finish(),
    )
    .expect("Failed to set default subscriber");

    GUI::run(Settings {
        window: window::Settings {
            size: (600, 190),
            ..Default::default()
        },
        ..Default::default()
    })
}

enum GatewayState {
    Connecting,
    Connected(Arc<Gateway>),
    Ready(Arc<Gateway>),
    Error,
}

enum VoiceState {
    Disconnected,
    Connecting,
    Connnected(Arc<Voice>),
    Error,
}

struct GUI {
    token: String,
    channel_id: String,
    gateway_state: GatewayState,
    voice_state: VoiceState,
    speaking: bool,
}

#[derive(Debug, Clone)]
enum Message {
    TokenChanged(String),
    ChannelIdChanged(String),
    LoginPressed,
    CallPressed,
    SpeakPressed,

    GatewayConnected(Result<Gateway, Error>),
    GatewayReady,
    CallStarted(Result<CallInformation, Error>),

    VoiceConnected(Result<Voice, Error>),
}

impl Application for GUI {
    type Message = Message;
    type Theme = Theme;
    type Executor = executor::Default;
    type Flags = ();

    fn new(_: ()) -> (Self, Command<Message>) {
        (
            Self {
                token: String::from(""),
                channel_id: String::from(""),
                gateway_state: GatewayState::Connecting,
                voice_state: VoiceState::Disconnected,
                speaking: false,
            },
            Command::perform(Gateway::new(), Message::GatewayConnected),
        )
    }

    fn title(&self) -> String {
        String::from("DiscordVoiceTest")
    }

    fn update(&mut self, message: Self::Message) -> Command<Message> {
        match message {
            Message::TokenChanged(token) => {
                self.token = token;
            }
            Message::ChannelIdChanged(id) => {
                self.channel_id = id;
            }
            Message::LoginPressed => {
                if let GatewayState::Connected(gateway) = &self.gateway_state {
                    return Command::perform(gateway.clone().identify(self.token.clone()), |_| {
                        Message::GatewayReady
                    });
                }
            }
            Message::CallPressed => {
                if let GatewayState::Ready(gateway) = &self.gateway_state {
                    return Command::perform(
                        gateway.clone().start_call(self.channel_id.clone()),
                        Message::CallStarted,
                    );
                }
            }
            Message::SpeakPressed => {
                if let VoiceState::Connnected(voice) = &self.voice_state {
                    self.speaking = !self.speaking;
                    tokio::spawn(voice.clone().set_sending(self.speaking));
                }
            }
            Message::GatewayConnected(gateway) => {
                if let Ok(gateway) = gateway {
                    self.gateway_state = GatewayState::Connected(Arc::new(gateway));
                } else {
                    self.gateway_state = GatewayState::Error;
                }
            }
            Message::GatewayReady => {
                if let GatewayState::Connected(gateway) = &self.gateway_state {
                    self.gateway_state = GatewayState::Ready(gateway.clone());
                }
            }
            Message::CallStarted(call_information) => {
                if let Ok(call_information) = call_information {
                    self.voice_state = VoiceState::Connecting;
                    return Command::perform(Voice::new(call_information), Message::VoiceConnected);
                } else {
                    self.voice_state = VoiceState::Error;
                }
            }
            Message::VoiceConnected(voice) => {
                if let Ok(voice) = voice {
                    self.voice_state = VoiceState::Connnected(Arc::new(voice));
                } else {
                    self.voice_state = VoiceState::Error;
                }
            }
        }

        Command::none()
    }

    fn view(&self) -> iced::Element<'_, Self::Message> {
        let gateway_text = match self.gateway_state {
            GatewayState::Connecting => text("Gateway connecting..."),
            GatewayState::Connected(_) => {
                text("Gateway connected").style(Color::from_rgb(0.1, 0.8, 0.35))
            }
            GatewayState::Ready(_) => text("Gateway ready").style(Color::from_rgb(0.1, 0.8, 0.35)),
            GatewayState::Error => text("Gateway error").style(Color::from_rgb(0.8, 0.2, 0.2)),
        }
        .size(16);

        let voice_text = match self.voice_state {
            VoiceState::Disconnected => text("Voice disconnected"),
            VoiceState::Connecting => text("Voice connecting..."),
            VoiceState::Connnected(_) => {
                text("Voice connected").style(Color::from_rgb(0.1, 0.8, 0.35))
            }
            VoiceState::Error => text("Voice error").style(Color::from_rgb(0.8, 0.2, 0.2)),
        }
        .size(16);

        let token_input = text_input("Token", &self.token, Message::TokenChanged)
            .padding(10)
            .size(20);

        let mut login_button = button("Login").padding(10).width(Length::Units(75));
        if let GatewayState::Connected(_) = self.gateway_state {
            login_button = login_button.on_press(Message::LoginPressed);
        }

        let channel_id_input =
            text_input("Channel ID", &self.channel_id, Message::ChannelIdChanged)
                .padding(10)
                .size(20);

        let mut call_button = button("Call").padding(10).width(Length::Units(75));
        if let GatewayState::Ready(_) = &self.gateway_state {
            call_button = call_button.on_press(Message::CallPressed);
        }

        let mut speak_button = if self.speaking {
            button("Stop speaking").style(Button::Destructive)
        } else {
            button("Start speaking")
        }
        .padding(10);

        if let VoiceState::Connnected(_) = self.voice_state {
            speak_button = speak_button.on_press(Message::SpeakPressed);
        }

        let content = column![
            row![gateway_text, voice_text].spacing(30),
            row![token_input, login_button].spacing(10),
            row![channel_id_input, call_button].spacing(10),
            speak_button
        ]
        .spacing(10)
        .padding(20);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x()
            .center_y()
            .into()
    }

    fn theme(&self) -> iced::Theme {
        Theme::Dark
    }
}

#[derive(Debug, Clone)]
pub enum Error {
    ApplicationError,
    SocketError,
    GatewayError,
}

impl From<tokio_tungstenite::tungstenite::Error> for Error {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        error!("Gateway error: {err}");

        Self::GatewayError
    }
}
