extern crate crypto;
extern crate env_logger;
extern crate futures;
extern crate getopts;
extern crate librespot;
#[macro_use]
extern crate log;
extern crate rpassword;
extern crate tokio_core;
extern crate tokio_io;
extern crate tokio_signal;
extern crate url;
#[macro_use]
extern crate serde_json;

use crypto::digest::Digest;
use crypto::sha1::Sha1;
use env_logger::{fmt, Builder};
// use futures::sync::mpsc::Receiver;
use std::sync::mpsc::{Receiver,channel};
use futures::{Async, Future, Poll, Stream};
use std::env;
use std::io::{self, stderr, Write};
use std::mem;
use std::path::PathBuf;
use std::process::exit;
use std::str::FromStr;
use tokio_core::reactor::{Core, Handle};
use tokio_io::IoStream;
use url::Url;

use librespot::core::authentication::{get_credentials, Credentials};
use librespot::core::cache::Cache;
use librespot::core::config::{ConnectConfig, DeviceType, SessionConfig};
use librespot::core::session::Session;
use librespot::core::version;

use librespot::connect::discovery::{discovery, DiscoveryStream};
use librespot::connect::spirc::{Spirc, SpircTask};
use librespot::playback::audio_backend::{self, Sink, BACKENDS};
use librespot::playback::config::{Bitrate, PlayerConfig};
use librespot::playback::mixer::{self, Mixer};
use librespot::playback::player::Player;

use librespot::core::events::Event;
// mod event_hooks;
// use event_hooks::handle_events;

mod meta_pipe;
use meta_pipe::{MetaPipe, MetaPipeConfig};

include!(concat!(env!("OUT_DIR"), "/version.rs"));

fn device_id(name: &str) -> String {
    let mut h = Sha1::new();
    h.input_str(name);
    h.result_str()
}

fn usage(program: &str, opts: &getopts::Options) -> String {
    let brief = format!("Usage: {} [options]", program);
    opts.usage(&brief)
}

fn setup_logging(verbose: bool) {
    let mut builder = Builder::new();
    builder.format(|buf, record| {
        let mut base_style = buf.style();
        let mut module_style = buf.style();
        let mut level_style = buf.style();
        let mut module_path = "";
        let level = record.level();

        match level {
            log::Level::Trace | log::Level::Debug => {
                module_path = record.module_path().unwrap_or("vollibrespot");
                module_style.set_color(fmt::Color::Yellow).set_bold(true);
                level_style.set_color(fmt::Color::Green)
            }
            log::Level::Info => level_style.set_color(fmt::Color::White),
            log::Level::Warn => level_style.set_color(fmt::Color::Yellow),
            log::Level::Error => level_style.set_color(fmt::Color::Red),
        };
        level_style.set_bold(true);
        base_style.set_color(fmt::Color::Cyan).set_bold(true);
        writeln!(
            buf,
            "{} {}: {}",
            base_style.value("vollibrespot"),
            module_style.value(module_path),
            level_style.value(record.args())
        )
    });
    match env::var("RUST_LOG") {
        Ok(config) => {
            builder.parse(&config);
            // env::set_var("RUST_LOG",&config);
            if verbose {
                warn!("`--verbose` flag overidden by `RUST_LOG` environment variable");
            }
            builder.init();
        }
        Err(_) => {
            if verbose {
                // env::set_var("RUST_LOG","mdns=info,librespot=info,vollibrespot=trace")
                builder.parse("mdns=info,librespot=info,vollibrespot=trace");
            } else {
                // env::set_var("RUST_LOG","mdns=info,vollibrespot=info")
                builder.parse("mdns=info,vollibrespot=info");
            }
            builder.init();
        }
    }
}

fn list_backends() {
    println!("Available Backends : ");
    for (&(name, _), idx) in BACKENDS.iter().zip(0..) {
        if idx == 0 {
            println!("- {} (default)", name);
        } else {
            println!("- {}", name);
        }
    }
}

#[derive(Clone)]
struct Setup {
    backend: fn(Option<String>) -> Box<Sink>,
    device: Option<String>,

    mixer: fn() -> Box<Mixer>,

    cache: Option<Cache>,
    player_config: PlayerConfig,
    session_config: SessionConfig,
    connect_config: ConnectConfig,
    meta_config: MetaPipeConfig,
    credentials: Option<Credentials>,
    enable_discovery: bool,
    zeroconf_port: u16,
    player_event_program: Option<String>,
}

fn setup(args: &[String]) -> Setup {
    let mut opts = getopts::Options::new();
    opts.optopt(
        "c",
        "cache",
        "Path to a directory where files will be cached.",
        "CACHE",
    ).optflag("", "disable-audio-cache", "Disable caching of the audio data.")
        .reqopt("n", "name", "Device name", "NAME")
        .optopt("", "device-type", "Displayed device type", "DEVICE_TYPE")
        .optopt(
            "b",
            "bitrate",
            "Bitrate (96, 160 or 320). Defaults to 160",
            "BITRATE",
        )
        .optopt(
            "",
            "onevent",
            "Run PROGRAM when playback is about to begin.",
            "PROGRAM",
        )
        .optflag("v", "verbose", "Enable verbose output")
        .optopt("u", "username", "Username to sign in with", "USERNAME")
        .optopt("p", "password", "Password", "PASSWORD")
        .optopt("", "proxy", "HTTP proxy to use when connecting", "PROXY")
        .optopt("", "ap-port", "Connect to AP with specified port. If no AP with that port are present fallback AP will be used. Available ports are usually 80, 443 and 4070", "AP_PORT")
        .optflag("", "disable-discovery", "Disable discovery mode")
        .optopt(
            "",
            "backend",
            "Audio backend to use. Use '?' to list options",
            "BACKEND",
        )
        .optopt(
            "",
            "device",
            "Audio device to use. Use '?' to list options if using portaudio",
            "DEVICE",
        )
        .optopt("", "mixer", "Mixer to use", "MIXER")
        .optopt(
            "",
            "initial-volume",
            "Initial volume in %, once connected (must be from 0 to 100)",
            "VOLUME",
        )
        .optopt(
            "",
            "zeroconf-port",
            "The port the internal server advertised over zeroconf uses.",
            "ZEROCONF_PORT",
        )
        .optflag(
            "",
            "enable-volume-normalisation",
            "Play all tracks at the same volume",
        )
        .optopt(
            "",
            "normalisation-pregain",
            "Pregain (dB) applied by volume normalisation",
            "PREGAIN",
        )
        .optflag(
            "",
            "linear-volume",
            "increase volume linear instead of logarithmic.",
        )
        .optopt(
            "",
            "metadata-port",
            "The port the metadata pipe uses.",
            "METADATA_PORT",
        );

    let matches = match opts.parse(&args[1..]) {
        Ok(m) => m,
        Err(f) => {
            writeln!(stderr(), "error: {}\n{}", f.to_string(), usage(&args[0], &opts)).unwrap();
            exit(1);
        }
    };

    let verbose = matches.opt_present("verbose");
    setup_logging(verbose);
    info!(
        "vollibrespot {} {} (librespot {} {}) -- Built On {}",
        short_sha(),
        commit_date(),
        version::short_sha(),
        version::commit_date(),
        short_now()
    );

    let backend_name = matches.opt_str("backend");
    if backend_name == Some("?".into()) {
        list_backends();
        exit(0);
    }

    let backend = audio_backend::find(backend_name).expect("Invalid backend");

    let device = matches.opt_str("device");

    let mixer_name = matches.opt_str("mixer");
    let mixer = mixer::find(mixer_name.as_ref()).expect("Invalid mixer");

    let use_audio_cache = !matches.opt_present("disable-audio-cache");

    let cache = matches
        .opt_str("c")
        .map(|cache_location| Cache::new(PathBuf::from(cache_location), use_audio_cache));

    let initial_volume = matches
        .opt_str("initial-volume")
        .map(|volume| {
            let volume = volume.parse::<u16>().unwrap();
            if volume > 100 {
                panic!("Initial volume must be in the range 0-100");
            }
            (volume as i32 * 0xFFFF / 100) as u16
        })
        .or_else(|| cache.as_ref().and_then(Cache::volume))
        .unwrap_or(0x8000);

    let zeroconf_port = matches
        .opt_str("zeroconf-port")
        .map(|port| port.parse::<u16>().unwrap())
        .unwrap_or(0);

    let name = matches.opt_str("name").unwrap();
    let use_audio_cache = !matches.opt_present("disable-audio-cache");

    let cache = matches
        .opt_str("c")
        .map(|cache_location| Cache::new(PathBuf::from(cache_location), use_audio_cache));

    let credentials = {
        let cached_credentials = cache.as_ref().and_then(Cache::credentials);

        let password = |username: &String| -> String {
            write!(stderr(), "Password for {}: ", username).unwrap();
            stderr().flush().unwrap();
            rpassword::read_password().unwrap()
        };

        get_credentials(
            matches.opt_str("username"),
            matches.opt_str("password"),
            cached_credentials,
            password,
        )
    };

    let session_config = {
        let device_id = device_id(&name);

        SessionConfig {
            user_agent: version::version_string(),
            device_id: device_id,
            proxy: matches.opt_str("proxy").or(std::env::var("http_proxy").ok()).map(
                |s| {
                    match Url::parse(&s) {
                Ok(url) => {
                    if url.host().is_none() || url.port().is_none() {
                        panic!("Invalid proxy url, only urls on the format \"http://host:port\" are allowed");
                    }

                    if url.scheme() != "http" {
                        panic!("Only unsecure http:// proxies are supported");
                    }
                    url
                },
                Err(err) => panic!("Invalid proxy url: {}, only urls on the format \"http://host:port\" are allowed", err)
            }
                },
            ),
            ap_port: matches
                .opt_str("ap-port")
                .map(|port| port.parse::<u16>().expect("Invalid port")),
        }
    };

    let player_config = {
        let bitrate = matches
            .opt_str("b")
            .as_ref()
            .map(|bitrate| Bitrate::from_str(bitrate).expect("Invalid bitrate"))
            .unwrap_or(Bitrate::default());

        PlayerConfig {
            bitrate: bitrate,
            normalisation: matches.opt_present("enable-volume-normalisation"),
            normalisation_pregain: matches
                .opt_str("normalisation-pregain")
                .map(|pregain| pregain.parse::<f32>().expect("Invalid pregain float value"))
                .unwrap_or(PlayerConfig::default().normalisation_pregain),
        }
    };

    let connect_config = {
        let device_type = matches
            .opt_str("device-type")
            .as_ref()
            .map(|device_type| DeviceType::from_str(device_type).expect("Invalid device type"))
            .unwrap_or(DeviceType::default());

        ConnectConfig {
            name: name,
            device_type: device_type,
            volume: initial_volume,
            linear_volume: matches.opt_present("linear-volume"),
        }
    };

    let meta_config = {
        let port = matches
            .opt_str("metadata-port")
            .map(|port| port.parse::<u16>().unwrap())
            .unwrap_or(5030);
        let version = format!(
            "vollibrespot {} {} (librespot {} {}) -- Built On {}",
            short_sha(),
            commit_date(),
            version::short_sha(),
            version::commit_date(),
            short_now()
        );

        MetaPipeConfig {
            port: port,
            version: version,
        }
    };

    let enable_discovery = !matches.opt_present("disable-discovery");

    Setup {
        backend: backend,
        cache: cache,
        session_config: session_config,
        player_config: player_config,
        connect_config: connect_config,
        meta_config: meta_config,
        credentials: credentials,
        device: device,
        enable_discovery: enable_discovery,
        zeroconf_port: zeroconf_port,
        mixer: mixer,
        player_event_program: matches.opt_str("onevent"),
    }
}

struct Main {
    cache: Option<Cache>,
    player_config: PlayerConfig,
    session_config: SessionConfig,
    connect_config: ConnectConfig,
    meta_config: MetaPipeConfig,
    backend: fn(Option<String>) -> Box<Sink>,
    device: Option<String>,
    mixer: fn() -> Box<Mixer>,
    handle: Handle,

    discovery: Option<DiscoveryStream>,
    signal: IoStream<()>,

    spirc: Option<Spirc>,
    spirc_task: Option<SpircTask>,
    connect: Box<Future<Item = Session, Error = io::Error>>,

    shutdown: bool,

    player_event_program: Option<String>,

    session: Option<Session>,
    event_channel: Option<Receiver<Event>>,
    meta_pipe: Option<MetaPipe>,
}

impl Main {
    fn new(handle: Handle, setup: Setup) -> Main {
        let mut task = Main {
            handle: handle.clone(),
            cache: setup.cache,
            session_config: setup.session_config,
            player_config: setup.player_config,
            connect_config: setup.connect_config,
            meta_config: setup.meta_config,
            backend: setup.backend,
            device: setup.device,
            mixer: setup.mixer,

            connect: Box::new(futures::future::empty()),
            discovery: None,
            spirc: None,
            spirc_task: None,
            shutdown: false,
            signal: Box::new(tokio_signal::ctrl_c(&handle).flatten_stream()),

            player_event_program: setup.player_event_program,

            event_channel: None,
            session: None,
            meta_pipe: None,
        };

        if setup.enable_discovery {
            let config = task.connect_config.clone();
            let device_id = task.session_config.device_id.clone();

            task.discovery = Some(discovery(&handle, config, device_id, setup.zeroconf_port).unwrap());
        }

        if let Some(credentials) = setup.credentials {
            task.credentials(credentials);
        }

        task
    }

    fn credentials(&mut self, credentials: Credentials) {
        let config = self.session_config.clone();
        let handle = self.handle.clone();

        let connection = Session::connect(config, credentials, self.cache.clone(), handle);

        self.connect = connection;
        self.spirc = None;
        let task = mem::replace(&mut self.spirc_task, None);
        if let Some(task) = task {
            self.handle.spawn(task);
        }
    }
}

impl Future for Main {
    type Item = ();
    type Error = ();

    fn poll(&mut self) -> Poll<(), ()> {
        loop {
            let mut progress = false;

            if let Some(Async::Ready(Some(creds))) = self.discovery.as_mut().map(|d| d.poll().unwrap()) {
                if let Some(ref spirc) = self.spirc {
                    spirc.shutdown();
                }
                self.credentials(creds);

                progress = true;
            }

            if let Async::Ready(session) = self.connect.poll().unwrap() {
                self.connect = Box::new(futures::future::empty());
                let device = self.device.clone();
                let mixer = (self.mixer)();
                let player_config = self.player_config.clone();
                let connect_config = self.connect_config.clone();
                let meta_config = self.meta_config.clone();

                // For event hooks
                // let (event_sender, event_receiver) = futures::sync::mpsc::unbounded::<Event>();
                let (event_sender, event_receiver) = channel();

                let audio_filter = mixer.get_audio_filter();
                let backend = self.backend;
                let player = Player::new(
                    player_config,
                    session.clone(),
                    event_sender.clone(),
                    audio_filter,
                    move || (backend)(device),
                );

                let (spirc, spirc_task) =
                    Spirc::new(connect_config, session.clone(), player, mixer, event_sender);

                let meta_pipe = MetaPipe::new(meta_config, session.clone(), event_receiver);

                self.spirc = Some(spirc);
                self.spirc_task = Some(spirc_task);
                self.session = Some(session);
                // self.event_channel = Some(event_receiver);
                self.meta_pipe = Some(meta_pipe);

                progress = true;
            }

            if let Async::Ready(Some(())) = self.signal.poll().unwrap() {
                if !self.shutdown {
                    if let Some(ref spirc) = self.spirc {
                        spirc.shutdown();
                    }

                    // if let Some(ref meta_pipe) = self.meta_pipe {
                    //     drop(meta_pipe);
                    // }

                    self.shutdown = true;
                } else {
                    return Ok(Async::Ready(()));
                }

                progress = true;
            }

            if let Some(ref mut spirc_task) = self.spirc_task {
                if let Async::Ready(()) = spirc_task.poll().unwrap() {
                    if self.shutdown {
                        return Ok(Async::Ready(()));
                    } else {
                        panic!("Spirc shut down unexpectedly");
                    }
                }
            }

            // if let Some(ref mut meta_task) = self.meta_task {
            //     if let Async::Ready(()) = meta_task.poll().unwrap() {
            //         return Ok(Async::Ready(()));
            //     }
            //     // if let Async::Ready(Some(event)) = event_channel.poll().unwrap() {
            //     //     // handle_events(event, self.session.clone().unwrap());
            //     // }
            // }

            if !progress {
                return Ok(Async::NotReady);
            }
        }
    }
}

fn main() {
    if env::var("RUST_BACKTRACE").is_err() {
        env::set_var("RUST_BACKTRACE", "full")
    }
    let mut core = Core::new().unwrap();
    let handle = core.handle();

    let args: Vec<String> = std::env::args().collect();

    core.run(Main::new(handle, setup(&args))).unwrap()
}