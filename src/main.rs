/*
 *  notesock terminal pastebin server
 *  Copyright (C) 2023 github.com/Sir-Photch
 *
 *  This program is free software: you can redistribute it and/or modify
 *  it under the terms of the GNU Affero General Public License as published
 *  by the Free Software Foundation, either version 3 of the License, or
 *  (at your option) any later version.
 *
 *  This program is distributed in the hope that it will be useful,
 *  but WITHOUT ANY WARRANTY; without even the implied warranty of
 *  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 *  GNU Affero General Public License for more details.
 *
 *  You should have received a copy of the GNU Affero General Public License
 *  along with this program.  If not, see <https://www.gnu.org/licenses/>.
 *
 */
#![cfg_attr(feature = "bench", feature(test))]

mod id_gen;
use clap_verbosity_flag::{InfoLevel, Verbosity};
use id_gen::*;

use clap::Parser;

use rand::prelude::*;
use simplelog::*;
use socket2::{Domain, SockAddr, Socket, Type};
use std::collections::HashSet;
use std::fs::{self, Permissions};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str;
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use std::io::{BufReader, Read, Write};

use crate::id_gen::IdGenerator;

const CARGO_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug, Clone)]
#[command(author, version)]
struct Args {
    #[arg(short = 's', long = "sockdir", default_value_t = String::from("/run/notesock"))]
    socket_dir: String,
    #[arg(short = 'm', long = "mode", default_value_t = 0o660)]
    socket_mode: u32,
    #[arg(short = 'H', long = "host", default_value_t = String::from("http://localhost"))]
    host: String,
    #[arg(short = 'w', long = "workers", default_value_t = 2)]
    workers: usize,
    #[arg(short = 'M', long = "max-size-kib", default_value_t = 512)]
    paste_len_kib: usize,
    #[arg(short = 't', long = "timeout-ms", default_value_t = 2000)]
    read_timeout: u64,
    #[arg(short = 'd', long = "directory", default_value_t = String::from("/var/lib/notesock"))]
    paste_dir: String,
    #[arg(short = 'c', long = "cleanup-after-sec", default_value_t = 240)]
    paste_expiry_sec: u64,
    #[arg(long = "no-cleanup", default_value_t = false)]
    no_clean_pastedir_on_start: bool,
    #[arg(short = 'l', long = "id-lower", default_value_t = String::from("1000"))]
    id_range_lower: String,
    #[arg(short = 'u', long = "id-upper", default_value_t = String::from("zzzz"))]
    id_range_upper: String,
    #[arg(long = "talk-proxy", default_value_t = false)]
    talk_proxy: bool,
    #[command(flatten)]
    verbose: Verbosity<InfoLevel>,
}

type SafeGen = Arc<Mutex<RandomIdGenerator<usize>>>;

const CLEANUP_WORKER_TAG: &str = "🧹";

const SOCKET_FILENAME: &str = "note.sock";

fn cleanup_worker(rx_cleanup: mpsc::Receiver<(Instant, PathBuf)>, ids: SafeGen) {
    loop {
        match rx_cleanup.recv() {
            Err(why) => error!("{} | rx_cleanup.recv: {}", CLEANUP_WORKER_TAG, why),
            Ok((next_timestamp, paste_path)) => {
                let now = Instant::now();
                if now < next_timestamp {
                    sleep(next_timestamp.duration_since(now));
                }

                match fs::remove_dir_all(&paste_path) {
                    Ok(()) => {
                        info!(
                            "{} | Cleaned up '{}'",
                            CLEANUP_WORKER_TAG,
                            paste_path.display()
                        );

                        // these checks are not necessary for release builds since
                        // workers panicking would cause the program to abort.
                        // still, I'm keeping the verbosity here
                        ids.lock()
                            .map(|mut lock| lock.remove(&paste_path.as_os_str().to_string_lossy()))
                            .map_err(|why| {
                                error!("{} | ids.lock.remove: {}", CLEANUP_WORKER_TAG, why)
                            })
                            .ok();
                    }
                    Err(why) => {
                        error!(
                            "{} | Cleanup failed '{}': {}",
                            CLEANUP_WORKER_TAG,
                            paste_path.display(),
                            why
                        )
                    }
                }
            }
        }
    }
}

fn paste_worker(
    tag: &str,
    rx_paste: spmc::Receiver<Socket>,
    gen: SafeGen,
    tx_clean: mpsc::Sender<(Instant, PathBuf)>,
    args: Args,
) {
    let paste_limit = args.paste_len_kib * 1024;
    let slack = if args.talk_proxy { 1024 } else { 0 } + 1;
    let paste_dir = Path::new(&args.paste_dir);
    let paste_timeout = Duration::from_secs(args.paste_expiry_sec);
    let exceeded_message = format!("Exceeded limit of {} kiB\n", args.paste_len_kib);
    let expiry_message = format!(
        "{}/_ID_ | 🧦 expires in {}\n",
        args.host,
        match args.paste_expiry_sec {
            x if x > 60 => match x % 60 {
                y if y > 0 => format!("{}m {}s", x / 60, y),
                _ => format!("{}m", x / 60),
            },
            x => format!("{}s", x),
        }
    );

    let mut buf = Vec::with_capacity(paste_limit + slack);

    let shutdown = |stream: &mut Socket, mode: Shutdown| {
        if mode == Shutdown::Write || mode == Shutdown::Both {
            stream.flush().ok();
        }
        stream
            .shutdown(mode)
            .map_err(|why| debug!("{} | {:?}: {}", tag, mode, why))
            .ok()
    };
    let reply = |stream: &mut Socket, message: &str| {
        stream
            .write_all(message.as_bytes())
            .map_err(|why| debug!("{} | reply error: {}", tag, why))
            .ok();
    };

    loop {
        let mut stream = match rx_paste.recv() {
            Ok(stream) => stream,
            Err(why) => {
                debug!("{} | rx.recv: {}", tag, why);
                continue;
            }
        };

        stream
            .set_read_timeout(Some(Duration::from_millis(args.read_timeout)))
            .map_err(|why| debug!("{} | set_read_timeout: {}", tag, why))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_millis(args.read_timeout)))
            .map_err(|why| debug!("{} | set_write_timeout: {}", tag, why))
            .ok();

        buf.clear();

        let msg_size = match BufReader::new(&stream)
            .take(paste_limit as u64 + slack as u64)
            .read_to_end(&mut buf)
        {
            Ok(read) => read,
            Err(why) => {
                debug!("{} | take.read_to_end: {}", tag, why);
                shutdown(&mut stream, Shutdown::Both);
                continue;
            }
        };

        shutdown(&mut stream, Shutdown::Read);

        let (mut header_len, mut payload_len) = (0, msg_size);

        if args.talk_proxy {
            let msg_len = buf.len();

            let mut slice = &buf.as_mut_slice()[..];
            match proxy_protocol::parse(&mut slice) {
                Ok(header) => info!(
                    "{} | {} kiB incoming | {:?}",
                    tag,
                    msg_size as f32 / 1024.0,
                    header
                ),
                Err(why) => {
                    debug!("{} | proxy_protocol.parse: {}", tag, why);
                    shutdown(&mut stream, Shutdown::Write);
                    continue;
                }
            }

            payload_len = slice.len();
            header_len = msg_len - payload_len;

            #[cfg(debug_assertions)]
            {
                assert!(msg_len != payload_len);
                trace!(
                    "{} | msg({}) | header({}): {:?} | payload({}): {:?}",
                    tag,
                    msg_len,
                    header_len,
                    std::str::from_utf8(&buf[..header_len]),
                    payload_len,
                    std::str::from_utf8(&buf[header_len..]).map(|p| {
                        if p.len() > 32 {
                            p[..29].to_owned() + "..."
                        } else {
                            p.to_string()
                        }
                    })
                )
            }
        }

        if payload_len > paste_limit {
            warn!("{} | exceeded paste limit", tag);
            reply(&mut stream, &exceeded_message);
            shutdown(&mut stream, Shutdown::Write);
            continue;
        }

        let payload = match std::str::from_utf8(&buf[header_len..]) {
            Ok(pld) => pld,
            Err(why) => {
                warn!("{} | invalid utf-8: {}", tag, why);
                reply(&mut stream, "invalid utf-8\n");
                shutdown(&mut stream, Shutdown::Write);
                continue;
            }
        };

        let mut gen = gen.lock().expect("Some thread has crashed!");

        let paste_id = match gen.get() {
            Some(id) => id,
            None => {
                // no ID can be generated, "address space is full"
                warn!(
                    "{} | Exhausted id generation in ({},{})",
                    tag, args.id_range_lower, args.id_range_upper
                );
                reply(
                    &mut stream,
                    "server is currently not accepting new pastes. try again later.\n",
                );
                shutdown(&mut stream, Shutdown::Write);
                continue;
            }
        };

        let paste_dir_path = paste_dir.join(&paste_id);

        match fs::create_dir_all(&paste_dir_path).and_then(|()| {
            let paste_path = paste_dir_path.join("index.txt");
            fs::write(&paste_path, payload)?;
            Ok(paste_path)
        }) {
            Ok(paste_path) => {
                info!("{} | saved paste to {}", tag, paste_path.display());
                tx_clean
                    .send((Instant::now() + paste_timeout, paste_dir_path))
                    .expect("Where did my cleanup task go?"); // if we can't cleanup anymore, it is time to panic!
            }
            Err(why) => {
                gen.remove(&paste_id);
                error!("{} | write-to-disk error: {}", tag, why);
                reply(&mut stream, "an internal error has occurred");
                shutdown(&mut stream, Shutdown::Write);
                continue;
            }
        }

        drop(gen);
        reply(&mut stream, &expiry_message.replace("_ID_", &paste_id));
        shutdown(&mut stream, Shutdown::Write);
    }
}

fn main() {
    let args = Args::parse();

    let socket_path = Path::new(&args.socket_dir);
    let paste_path = Path::new(&args.paste_dir);

    if !socket_path
        .try_exists()
        .expect("Can't acces socket directory path")
    {
        fs::create_dir_all(socket_path).expect("Can't create socket directory");
    }

    if !paste_path.try_exists().expect("Can't access paste path") {
        fs::create_dir_all(paste_path).expect("Can't create paste directory");
    }

    let paste_id_regex =
        regex::Regex::new(&format!("{}{{{},}}", ID_REGEXP, args.id_range_lower.len()))
            .expect("Regex compilation failed");

    let id_set: HashSet<_> = fs::read_dir(paste_path)
        .expect("Can't access paste dir")
        .filter_map(|f| {
            let entry = f.ok()?;
            if !entry.file_type().ok()?.is_dir() {
                return None;
            }

            let name = entry.file_name();
            if !paste_id_regex.is_match(&name.to_string_lossy()) {
                return None;
            }

            Some(name)
        })
        .collect();

    let id_set = if id_set.is_empty() {
        None
    } else {
        Some(id_set)
    };

    drop(paste_id_regex);

    let socket_path = socket_path.join(SOCKET_FILENAME);
    if socket_path
        .try_exists()
        .expect("Can't access socket descriptor path")
    {
        fs::remove_file(&socket_path).expect("Can't unlink existing socket");
    }

    let socket = Socket::new(Domain::UNIX, Type::STREAM, None).expect("Could not create socket");
    socket
        .bind(&SockAddr::unix(&socket_path).expect("Bad socket address"))
        .expect("Could not bind socket");
    fs::set_permissions(&socket_path, Permissions::from_mode(args.socket_mode))
        .expect("Could not set socket permission");
    socket
        .set_nonblocking(false)
        .expect("Could not set socket to blocking");
    socket
        .listen(args.workers as i32 * 2)
        .expect("Could not start listening");

    CombinedLogger::init(vec![TermLogger::new(
        args.verbose.log_level_filter(),
        Config::default(),
        TerminalMode::Stdout,
        ColorChoice::Auto,
    )])
    .unwrap();

    info!(
        "Starting notesock v{} on <b>{}</b> 🧦",
        CARGO_VERSION,
        socket_path
            .canonicalize()
            .expect("Bad socket path")
            .display()
    );

    if let Some(ref set) = id_set {
        if !args.no_clean_pastedir_on_start {
            for f in set.iter() {
                fs::remove_dir_all(paste_path.join(f))
                    .map(|()| info!("Cleaned up old {:?}", f))
                    .map_err(|why| error!("Could not clean up '{:?}': {}", f, why))
                    .ok();
            }
        }
    }
    let id_set = id_set.map(|set| {
        set.iter()
            .filter_map(|v| v.to_str().map(|v| v.to_owned()))
            .collect::<HashSet<String>>()
    });

    let generator = Arc::new(Mutex::new(
        RandomIdGenerator::<usize>::new(
            &args.id_range_lower,
            &args.id_range_upper,
            Some(256),
            id_set,
        )
        .expect("Could not create id generator"),
    ));

    let (mut tx_paste, rx_paste) = spmc::channel();
    let (tx_cleanup, rx_cleanup) = mpsc::channel();

    let worker_tags: Vec<_> = emojis::Group::FoodAndDrink
        .emojis()
        .map(|e| e.as_str())
        .choose_multiple(&mut thread_rng(), args.workers);

    info!("Spawning workers: {}", worker_tags.join(" | "));

    for tag in worker_tags {
        let args = args.clone();
        let id_set = generator.clone();
        let rx_paste = rx_paste.clone();
        let tx_cleanup = tx_cleanup.clone();
        thread::spawn(move || paste_worker(tag, rx_paste, id_set, tx_cleanup, args));
    }

    thread::spawn(|| cleanup_worker(rx_cleanup, generator));

    loop {
        match socket.accept() {
            Ok((socket, _addr)) => tx_paste.send(socket).expect("All my workers are gone!"),
            Err(why) => warn!("accept failed: {}", why),
        }
    }
}
