#![feature(plugin,scoped)]
#![allow(deprecated)]
//#![allow(unused_imports,dead_code)]

#![plugin(protobuf_macros)]
#[macro_use] extern crate lazy_static;


extern crate byteorder;
extern crate crypto;
extern crate gmp;
extern crate num;
extern crate portaudio;
extern crate protobuf;
extern crate shannon;
extern crate rand;
extern crate readall;
extern crate vorbis;

extern crate librespot_protocol;
#[macro_use] extern crate librespot;

use std::clone::Clone;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use protobuf::core::Message;
use std::thread;
use std::path::PathBuf;

use librespot::util;
use librespot::metadata::{AlbumRef, ArtistRef, TrackRef};
use librespot::session::{Config, Session};
use librespot::util::SpotifyId;
use librespot::util::version::version_string;
use librespot::player::{Player, PlayerCommand};
use librespot::mercury::{MercuryRequest, MercuryMethod};

use librespot_protocol as protocol;
use librespot_protocol::spirc::PlayStatus;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut appkey_file = File::open(Path::new(&args.next().unwrap())).unwrap();
    let username = args.next().unwrap();
    let password = args.next().unwrap();
    let cache_location = args.next().unwrap();
    let name = args.next().unwrap();

    let mut appkey = Vec::new();
    appkey_file.read_to_end(&mut appkey).unwrap();

    let config = Config {
        application_key: appkey,
        user_agent: version_string(),
        device_id: name.clone(),
        cache_location: PathBuf::from(cache_location)
    };
    let session = Session::new(config);
    session.login(username.clone(), password);
    session.poll();

    let poll_thread = thread::scoped(|| {
        loop {
            session.poll();
        }
    });

    let player = Player::new(&session);

    SpircManager {
        session: &session,
        player: &player,

        username: username.clone(),
        state_update_id: 0,
        seq_nr: 0,

        name: name,
        ident: session.config.device_id.clone(),
        device_type: 5,
        can_play: true,

        repeat: false,
        shuffle: false,
        volume: 0x8000,

        is_active: false,
        became_active_at: 0,

        last_command_ident: String::new(),
        last_command_msgid: 0,

        track: None
    }.run();

    poll_thread.join();
}

fn print_track(session: &Session, track_id: SpotifyId) {
    let track : TrackRef = session.metadata(track_id);

    let album : AlbumRef = {
        let handle = track.wait();
        let data = handle.unwrap();
        eprintln!("{}", data.name);
        session.metadata(data.album)
    };

    let artists : Vec<ArtistRef> = {
        let handle = album.wait();
        let data = handle.unwrap();
        eprintln!("{}", data.name);
        data.artists.iter().map(|id| {
            session.metadata(*id)
        }).collect()
    };

    for artist in artists {
        let handle = artist.wait();
        let data = handle.unwrap();
        eprintln!("{}", data.name);
    }
}

struct SpircManager<'s> {
    player: &'s Player<'s>,
    session: &'s Session,

    username: String,
    state_update_id: i64,
    seq_nr: u32,

    name: String,
    ident: String,
    device_type: u8,
    can_play: bool,

    repeat: bool,
    shuffle: bool,
    volume: u16,

    is_active: bool,
    became_active_at: i64,

    last_command_ident: String,
    last_command_msgid: u32,

    track: Option<SpotifyId>
}

impl <'s> SpircManager<'s> {
    fn run(&mut self) {
        let rx = self.session.mercury_sub(format!("hm://remote/user/{}/v23", self.username));

        self.notify(None);

        loop {
            if let Ok(pkt) = rx.try_recv() {
                let frame = protobuf::parse_from_bytes::<protocol::spirc::Frame>(
                    pkt.payload.front().unwrap()).unwrap();
                println!("{:?} {} {} {} {}",
                         frame.get_typ(),
                         frame.get_device_state().get_name(),
                         frame.get_ident(),
                         frame.get_seq_nr(),
                         frame.get_state_update_id());
                if frame.get_ident() != self.ident &&
                    (frame.get_recipient().len() == 0 ||
                     frame.get_recipient().contains(&self.ident)) {
                    self.handle(frame);
                }
            }

            let h = self.player.state.0.lock().unwrap();
            if h.update_time > self.state_update_id {
                self.state_update_id = util::now_ms();
                drop(h);
                self.notify(None);
            }
        }
    }

    fn handle(&mut self, frame: protocol::spirc::Frame) {
        if frame.get_recipient().len() > 0 {
            self.last_command_ident = frame.get_ident().to_string();
            self.last_command_msgid = frame.get_seq_nr();
        }
        match frame.get_typ() {
            protocol::spirc::MessageType::kMessageTypeHello => {
                self.notify(Some(frame.get_ident()));
            }
            protocol::spirc::MessageType::kMessageTypeLoad => {
                if !self.is_active {
                    self.is_active = true;
                    self.became_active_at = util::now_ms();
                }

                let index = frame.get_state().get_playing_track_index() as usize;
                let track = SpotifyId::from_raw(frame.get_state().get_track()[index].get_gid());
                self.track = Some(track);
                self.player.command(PlayerCommand::Load(track,
                                                        frame.get_state().get_status() == PlayStatus::kPlayStatusPlay,
                                                        frame.get_state().get_position_ms()));
            }
            protocol::spirc::MessageType::kMessageTypePlay => {
                self.player.command(PlayerCommand::Play);
            }
            protocol::spirc::MessageType::kMessageTypePause => {
                self.player.command(PlayerCommand::Pause);
            }
            protocol::spirc::MessageType::kMessageTypeSeek => {
                self.player.command(PlayerCommand::Seek(frame.get_position()));
            }
            protocol::spirc::MessageType::kMessageTypeNotify => {
                if self.is_active && frame.get_device_state().get_is_active() {
                    self.is_active = false;
                    self.player.command(PlayerCommand::Stop);
                }
            }
            _ => ()
        }
    }

    fn notify(&mut self, recipient: Option<&str>) {
        let mut pkt = protobuf_init!(protocol::spirc::Frame::new(), {
            version: 1,
            ident: self.ident.clone(),
            protocol_version: "2.0.0".to_string(),
            seq_nr: { self.seq_nr += 1; self.seq_nr  },
            typ: protocol::spirc::MessageType::kMessageTypeNotify,
            device_state: self.device_state(),
            recipient: protobuf::RepeatedField::from_vec(
                recipient.map(|r| vec![r.to_string()] ).unwrap_or(vec![])
            ),
            state_update_id: self.state_update_id
        });

        if self.is_active {
            pkt.set_state(self.state());
        }

        self.session.mercury(MercuryRequest{
            method: MercuryMethod::SEND,
            uri: format!("hm://remote/user/{}", self.username),
            content_type: None,
            payload: vec![ pkt.write_to_bytes().unwrap() ]
        });
    }

    fn state(&mut self) -> protocol::spirc::State {
        let state = self.player.state.0.lock().unwrap();

        protobuf_init!(protocol::spirc::State::new(), {
            status: state.status,
            position_ms: state.position_ms,
            position_measured_at: state.position_measured_at as u64,

            playing_track_index: 0,
            track => [
                @{
                    gid: self.track.unwrap().to_raw().to_vec()
                }
            ],

            shuffle: self.shuffle,
            repeat: self.repeat,

            playing_from_fallback: true,

            last_command_ident: self.last_command_ident.clone(),
            last_command_msgid: self.last_command_msgid
        })
    }

    fn device_state(&mut self) -> protocol::spirc::DeviceState {
        protobuf_init!(protocol::spirc::DeviceState::new(), {
            sw_version: version_string(),
            is_active: self.is_active,
            can_play: self.can_play,
            volume: self.volume as u32,
            name: self.name.clone(),
            error_code: 0,
            became_active_at: if self.is_active { self.became_active_at } else { 0 },
            capabilities => [
                @{
                    typ: protocol::spirc::CapabilityType::kCanBePlayer,
                    intValue => [0]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kDeviceType,
                    intValue => [ self.device_type as i64 ]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kGaiaEqConnectId,
                    intValue => [1]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kSupportsLogout,
                    intValue => [0]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kIsObservable,
                    intValue => [1]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kVolumeSteps,
                    intValue => [10]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kSupportedContexts,
                    stringValue => [
                        "album".to_string(),
                        "playlist".to_string(),
                        "search".to_string(),
                        "inbox".to_string(),
                        "toplist".to_string(),
                        "starred".to_string(),
                        "publishedstarred".to_string(),
                        "track".to_string(),
                    ]
                },
                @{
                    typ: protocol::spirc::CapabilityType::kSupportedTypes,
                    stringValue => [
                        "audio/local".to_string(),
                        "audio/track".to_string(),
                        "local".to_string(),
                        "track".to_string(),
                    ]
                }
            ],
        })
    }
}

