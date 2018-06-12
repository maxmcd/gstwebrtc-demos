extern crate clap;
extern crate failure;
extern crate glib;
extern crate gstreamer as gst;
extern crate gstreamer_sdp as gst_sdp;
extern crate gstreamer_webrtc as gst_webrtc;
extern crate rand;
extern crate serde;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;
extern crate ws;

use failure::Error;
use gst::prelude::*;
use gst::{BinExt, ElementExt};
use rand::Rng;
use std::sync::{Arc, Mutex};

#[derive(PartialEq, PartialOrd, Eq, Debug)]
enum AppState {
    // AppStateUnknown = 0,
    AppStateErr = 1,
    ServerConnecting = 1000,
    ServerConnectionError,
    ServerConnected,
    ServerRegistering = 2000,
    ServerRegisteringError,
    ServerRegistered,
    ServerClosed,
    PeerConnecting = 3000,
    PeerConnectionError,
    PeerConnected,
    PeerCallNegotiating = 4000,
    PeerCallStarted,
    PeerCallError,
}

const STUN_SERVER: &'static str = "stun://stun.l.google.com:19302 ";

fn rtp_caps_opus() -> gst::GstRc<gst::CapsRef> {
    gst::Caps::new_simple(
        "application/x-rtp",
        &[
            ("media", &"audio"),
            ("encoding-name", &"OPUS"),
            ("payload", &(97i32)),
        ],
    )
}
fn rtp_caps_vp8() -> gst::GstRc<gst::CapsRef> {
    gst::Caps::new_simple(
        "application/x-rtp",
        &[
            ("media", &"video"),
            ("encoding-name", &"VP8"),
            ("payload", &(96i32)),
        ],
    )
}

fn check_plugins() -> bool {
    let needed = vec![
        "opus",
        "vpx",
        "nice",
        "webrtc",
        "dtls",
        "srtp",
        "rtpmanager",
        "videotestsrc",
        "audiotestsrc",
    ];
    let registry = gst::Registry::get();
    let mut ret = true;
    for plugin_name in needed {
        let plugin = registry.find_plugin(&plugin_name.to_string());
        if plugin.is_none() {
            println!("Required gstreamer plugin '{}' not found", plugin_name);
            ret = false;
        }
    }
    ret
}

fn setup_call(app_control: &Arc<Mutex<AppControl>>) -> AppState {
    let mut app_control = app_control.lock().unwrap();
    app_control.app_state = AppState::PeerConnecting;
    println!(
        "Setting up signalling server call with {}",
        app_control.peer_id
    );
    app_control
        .ws_sender
        .send(format!("SESSION {}", app_control.peer_id))
        .unwrap();
    AppState::PeerConnecting
}

fn register_with_server(app_control: &Arc<Mutex<AppControl>>) -> AppState {
    let mut app_control = app_control.lock().unwrap();
    app_control.app_state = AppState::ServerRegistering;
    let our_id = rand::thread_rng().gen_range(10, 10_000);
    println!("Registering id {} with server", our_id);
    app_control
        .ws_sender
        .send(format!("HELLO {}", our_id))
        .unwrap();
    AppState::ServerRegistering
}

fn send_sdp_offer(
    app_control: &Arc<Mutex<AppControl>>,
    offer: gst_webrtc::WebRTCSessionDescription,
) {
    let app_control = app_control.lock().unwrap();
    if app_control.app_state < AppState::PeerCallNegotiating {
        // TODO signal and cleanup
        panic!("Can't send offer, not in call");
    };
    let message = json!({
      "sdp": {
        "type": "offer",
        "sdp": offer.get_sdp().as_text().unwrap(),
      }
    });
    app_control.ws_sender.send(message.to_string()).unwrap();
}

fn on_offer_created(
    app_control: &Arc<Mutex<AppControl>>,
    webrtc: gst::Element,
    promise: &gst::Promise,
) {
    assert_eq!(
        app_control.lock().unwrap().app_state,
        AppState::PeerCallNegotiating
    );
    let reply = promise.get_reply().unwrap();

    let offer = reply
        .get_value("offer")
        .unwrap()
        .get::<gst_webrtc::WebRTCSessionDescription>()
        .expect("Invalid argument");
    webrtc
        .emit("set-local-description", &[&offer, &None::<gst::Promise>])
        .unwrap();

    send_sdp_offer(app_control, offer)
}

fn on_negotiation_needed(
    app_control: &Arc<Mutex<AppControl>>,
    values: &[glib::Value],
) -> Option<glib::Value> {
    app_control.lock().unwrap().app_state = AppState::PeerCallNegotiating;
    let webrtc = values[0].get::<gst::Element>().expect("Invalid argument");
    let webrtc_clone = webrtc.clone();
    let app_control_clone = app_control.clone();
    let promise = gst::Promise::new_with_change_func(move |promise| {
        on_offer_created(&app_control_clone, webrtc, promise);
    });
    webrtc_clone
        .emit("create-offer", &[&None::<gst::Structure>, &promise])
        .unwrap();
    None
}

enum MediaType {
    Audio,
    Video,
}

fn handle_media_stream(
    pad: &gst::Pad,
    pipe: &gst::Pipeline,
    media_type: MediaType,
) -> Result<(), Error> {
    let (convert_name, sink_name) = match media_type {
        MediaType::Video => ("videoconvert", "autovideosink"),
        MediaType::Audio => ("audioconvert", "autoaudiosink"),
    };
    println!(
        "Trying to handle stream with {} ! {}",
        convert_name, sink_name,
    );
    let q = gst::ElementFactory::make("queue", None).unwrap();
    let conv = gst::ElementFactory::make(convert_name, None).unwrap();
    let sink = gst::ElementFactory::make(sink_name, None).unwrap();

    match media_type {
        MediaType::Audio => {
            let resample = gst::ElementFactory::make("audioresample", None).unwrap();
            pipe.add_many(&[&q, &conv, &resample, &sink])?;
            gst::Element::link_many(&[&q, &conv, &resample, &sink])?;
            resample.sync_state_with_parent()?;
        }
        MediaType::Video => {
            pipe.add_many(&[&q, &conv, &sink])?;
            gst::Element::link_many(&[&q, &conv, &sink])?;
        }
    };
    q.sync_state_with_parent()?;
    conv.sync_state_with_parent()?;
    sink.sync_state_with_parent()?;

    let qpad = q.get_static_pad("sink").unwrap();
    let ret = pad.link(&qpad);
    assert_eq!(ret, gst::PadLinkReturn::Ok);
    Ok(())
}

fn on_incoming_decodebin_stream(
    values: &[glib::Value],
    pipe: &gst::Pipeline,
) -> Option<glib::Value> {
    let pad = values[1].get::<gst::Pad>().expect("Invalid argument");
    if !pad.has_current_caps() {
        println!("Pad {:?} has no caps, can't do anything, ignoring", pad);
    }

    let caps = pad.get_current_caps().unwrap();
    let name = caps.get_structure(0).unwrap().get_name();
    match if name.starts_with("video") {
        handle_media_stream(&pad, &pipe, MediaType::Video)
    } else if name.starts_with("audio") {
        handle_media_stream(&pad, &pipe, MediaType::Audio)
    } else {
        println!("Unknown pad {:?}, ignoring", pad);
        Ok(())
    } {
        Ok(()) => return None,
        Err(err) => panic!("Error adding pad with caps {} {:?}", name, err),
    };
}

fn on_incoming_stream(values: &[glib::Value], pipe: &gst::Pipeline) -> Option<glib::Value> {
    let webrtc = values[0].get::<gst::Element>().expect("Invalid argument");
    let decodebin = gst::ElementFactory::make("decodebin", None).unwrap();
    let pipe_clone = pipe.clone();
    decodebin
        .connect("pad-added", false, move |values| {
            on_incoming_decodebin_stream(values, &pipe_clone)
        })
        .unwrap();
    pipe.clone()
        .dynamic_cast::<gst::Bin>()
        .unwrap()
        .add(&decodebin)
        .unwrap();
    decodebin.sync_state_with_parent().unwrap();
    webrtc.link(&decodebin).unwrap();
    None
}

fn send_ice_candidate_message(
    app_control: &Arc<Mutex<AppControl>>,
    values: &[glib::Value],
) -> Option<glib::Value> {
    let app_control = app_control.lock().unwrap();
    if app_control.app_state < AppState::PeerCallNegotiating {
        panic!("Can't send ICE, not in call");
    }

    let _webrtc = values[0].get::<gst::Element>().expect("Invalid argument");
    let mlineindex = values[1].get::<u32>().expect("Invalid argument");
    let candidate = values[2].get::<String>().expect("Invalid argument");
    let message = json!({
          "ice": {
            "candidate": candidate,
            "sdpMLineIndex": mlineindex,
          }
        });
    app_control.ws_sender.send(message.to_string()).unwrap();
    None
}

fn add_video_source(pipeline: &gst::Pipeline, webrtcbin: &gst::Element) -> Result<(), Error> {
    let videotestsrc = gst::ElementFactory::make("videotestsrc", None).unwrap();
    videotestsrc.set_property_from_str("pattern", "ball");
    let videoconvert = gst::ElementFactory::make("videoconvert", None).unwrap();
    let queue = gst::ElementFactory::make("queue", None).unwrap();
    let vp8enc = gst::ElementFactory::make("vp8enc", None).unwrap();
    vp8enc.set_property("deadline", &1i64)?;
    let rtpvp8pay = gst::ElementFactory::make("rtpvp8pay", None).unwrap();
    let queue2 = gst::ElementFactory::make("queue", None).unwrap();
    pipeline.add_many(&[
        &videotestsrc,
        &videoconvert,
        &queue,
        &vp8enc,
        &rtpvp8pay,
        &queue2,
    ])?;
    gst::Element::link_many(&[
        &videotestsrc,
        &videoconvert,
        &queue,
        &vp8enc,
        &rtpvp8pay,
        &queue2,
    ])?;
    queue2.link_filtered(webrtcbin, &rtp_caps_vp8())?;
    Ok(())
}

fn add_audio_source(pipeline: &gst::Pipeline, webrtcbin: &gst::Element) -> Result<(), Error> {
    let audiotestsrc = gst::ElementFactory::make("audiotestsrc", None).unwrap();
    audiotestsrc.set_property_from_str("wave", "red-noise");
    let queue = gst::ElementFactory::make("queue", None).unwrap();
    let audioconvert = gst::ElementFactory::make("audioconvert", None).unwrap();
    let audioresample = gst::ElementFactory::make("audioresample", None).unwrap();
    let queue2 = gst::ElementFactory::make("queue", None).unwrap();
    let opusenc = gst::ElementFactory::make("opusenc", None).unwrap();
    let rtpopuspay = gst::ElementFactory::make("rtpopuspay", None).unwrap();
    let queue3 = gst::ElementFactory::make("queue", None).unwrap();
    pipeline.add_many(&[
        &audiotestsrc,
        &queue,
        &audioconvert,
        &audioresample,
        &queue2,
        &opusenc,
        &rtpopuspay,
        &queue3,
    ])?;
    gst::Element::link_many(&[
        &audiotestsrc,
        &queue,
        &audioconvert,
        &audioresample,
        &queue2,
        &opusenc,
        &rtpopuspay,
        &queue3,
    ])?;
    queue3.link_filtered(webrtcbin, &rtp_caps_opus())?;
    Ok(())
}

fn construct_pipeline() -> Result<gst::Pipeline, Error> {
    let pipeline = gst::Pipeline::new(None);
    let webrtcbin = gst::ElementFactory::make("webrtcbin", "sendrecv").unwrap();
    pipeline.add(&webrtcbin)?;
    webrtcbin.set_property_from_str("stun-server", STUN_SERVER);
    add_video_source(&pipeline, &webrtcbin)?;
    add_audio_source(&pipeline, &webrtcbin)?;
    Ok(pipeline)
}

fn start_pipeline(app_control: &Arc<Mutex<AppControl>>) -> Result<gst::Element, Error> {
    let pipe = construct_pipeline()?;

    let webrtc = pipe.clone()
        .dynamic_cast::<gst::Bin>()
        .unwrap()
        .get_by_name("sendrecv")
        .unwrap();
    let app_control_clone = app_control.clone();
    webrtc.connect("on-negotiation-needed", false, move |values| {
        on_negotiation_needed(&app_control_clone, values)
    })?;

    let app_control_clone = app_control.clone();
    webrtc.connect("on-ice-candidate", false, move |values| {
        send_ice_candidate_message(&app_control_clone, values)
    })?;

    let pipe_clone = pipe.clone();
    webrtc.connect("pad-added", false, move |values| {
        on_incoming_stream(values, &pipe_clone)
    })?;

    pipe.set_state(gst::State::Playing).into_result()?;

    Ok(webrtc)
}

struct WsClient {
    webrtc: Option<gst::Element>,
    app_control: Arc<Mutex<AppControl>>,
}

struct AppControl {
    app_state: AppState,
    ws_sender: ws::Sender,
    peer_id: String,
}

impl WsClient {
    fn update_state(&self, state: AppState) {
        self.app_control.lock().unwrap().app_state = state
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum JsonMsg {
    Ice {
        candidate: String,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: u32,
    },
    Sdp {
        #[serde(rename = "type")]
        type_: String,
        sdp: String,
    },
}

impl ws::Handler for WsClient {
    fn on_open(&mut self, _: ws::Handshake) -> ws::Result<()> {
        self.update_state(AppState::ServerConnected);
        self.update_state(register_with_server(&self.app_control.clone()));
        Ok(())
    }

    fn on_message(&mut self, msg: ws::Message) -> ws::Result<()> {
        // Close the connection when we get a response from the server
        let msg_text = msg.into_text().unwrap();
        if msg_text == "HELLO" {
            if self.app_control.lock().unwrap().app_state != AppState::ServerRegistering {
                panic!("ERROR: Received HELLO when not registering");
            }
            self.update_state(AppState::ServerRegistered);
            setup_call(&self.app_control.clone());
            return Ok(());
        }
        if msg_text == "SESSION_OK" {
            if self.app_control.lock().unwrap().app_state != AppState::PeerConnecting {
                panic!("ERROR: Received SESSION_OK when not calling");
            }
            self.update_state(AppState::PeerConnected);
            self.webrtc = match start_pipeline(&self.app_control) {
                Ok(webrtc) => Some(webrtc),
                Err(err) => {
                    panic!("Failed to set up webrtc {:?}", err);
                }
            };
            return Ok(());
        }

        if msg_text.starts_with("ERROR") {
            println!("Got error message! {}", msg_text);
            let error = match self.app_control.lock().unwrap().app_state {
                AppState::ServerConnecting => AppState::ServerConnectionError,
                AppState::ServerRegistering => AppState::ServerRegisteringError,
                AppState::PeerConnecting => AppState::PeerConnectionError,
                AppState::PeerConnected => AppState::PeerCallError,
                AppState::PeerCallNegotiating => AppState::PeerCallError,
                AppState::ServerConnectionError => AppState::ServerConnectionError,
                AppState::ServerRegisteringError => AppState::ServerRegisteringError,
                AppState::PeerConnectionError => AppState::PeerConnectionError,
                AppState::PeerCallError => AppState::PeerCallError,
                AppState::AppStateErr => AppState::AppStateErr,
                AppState::ServerConnected => AppState::AppStateErr,
                AppState::ServerRegistered => AppState::AppStateErr,
                AppState::ServerClosed => AppState::AppStateErr,
                AppState::PeerCallStarted => AppState::AppStateErr,
            };
            self.app_control
                .lock()
                .unwrap()
                .ws_sender
                .close(ws::CloseCode::Normal)
                .unwrap();
            panic!("Got websocket error {:?}", error);
            // TODO: signal & cleanup
        }

        let json_msg: JsonMsg = serde_json::from_str(&msg_text).unwrap();
        match json_msg {
            JsonMsg::Sdp { type_, sdp } => {
                assert_eq!(
                    self.app_control.lock().unwrap().app_state,
                    AppState::PeerCallNegotiating
                );

                assert_eq!(type_, "answer");
                print!("Received answer:\n{}\n", sdp);

                let ret = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes()).unwrap();
                let answer = gst_webrtc::WebRTCSessionDescription::new(
                    gst_webrtc::WebRTCSDPType::Answer,
                    ret,
                );
                let promise = gst::Promise::new();
                self.webrtc
                    .as_ref()
                    .unwrap()
                    .emit("set-remote-description", &[&answer, &promise])
                    .unwrap();
                self.update_state(AppState::PeerCallStarted);
            }
            JsonMsg::Ice {
                sdp_mline_index,
                candidate,
            } => {
                self.webrtc
                    .as_ref()
                    .unwrap()
                    .emit("add-ice-candidate", &[&sdp_mline_index, &candidate])
                    .unwrap();
            }
        }

        Ok(())
    }

    fn on_close(&mut self, _code: ws::CloseCode, _reason: &str) {
        self.app_control.lock().unwrap().app_state = AppState::ServerClosed;
    }
}

fn connect_to_websocket_server_async(peer_id: &str, server: &str) {
    println!("Connecting to server {}", server);
    ws::connect(server, |ws_sender| WsClient {
        webrtc: None,
        app_control: Arc::new(Mutex::new(AppControl {
            ws_sender: ws_sender,
            peer_id: peer_id.to_string(),
            app_state: AppState::ServerConnecting,
        })),
    }).unwrap();
}

fn main() {
    let matches = clap::App::new("Sendrcv rust")
        .arg(
            clap::Arg::with_name("peer-id")
                .help("String ID of the peer to connect to")
                .long("peer-id")
                .required(true)
                .takes_value(true),
        )
        .arg(
            clap::Arg::with_name("server")
                .help("Signalling server to connect to")
                .long("server")
                .required(false)
                .takes_value(true),
        )
        .get_matches();

    gst::init().unwrap();

    if !check_plugins() {
        return;
    }
    let main_loop = glib::MainLoop::new(None, false);
    connect_to_websocket_server_async(
        matches.value_of("peer-id").unwrap(),
        matches
            .value_of("server")
            .unwrap_or("wss://webrtc.nirbheek.in:8443"),
    );
    main_loop.run();
}
