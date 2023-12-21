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

use clap::Parser;
use id_gen::PartitionIdGenerator;
use rand::prelude::*;
use simplelog::*;
use socket2::{Domain, SockAddr, Socket, Type};
use std::collections::HashSet;
use std::fs::{self, Permissions};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use std::io::{ErrorKind, Read, Write};

use crate::id_gen::IdGenerator;

const CARGO_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Debug, Clone)]
struct Args {
    #[arg(short = 's', long = "sockdir", default_value_t = String::from("/run/notesock"))]
    socket_dir: String,
    #[arg(short = 'm', long = "mode", default_value_t = 0o660)]
    socket_mode: u32,
    #[arg(long = "host", default_value_t = String::from("http://localhost"))]
    host: String,
    #[arg(short = 'w', default_value_t = 2)]
    workers: usize,
    #[arg(short = 'l', long = "max-size-kb", default_value_t = 500)]
    paste_len_kb: usize,
    #[arg(short = 't', long = "timeout-ms", default_value_t = 2000)]
    read_timeout: u64,
    #[arg(short = 'd', long = "directory", default_value_t = String::from("/var/lib/notesock"))]
    paste_dir: String,
    #[arg(short = 'x', long = "cleanup-after-sec", default_value_t = 240)]
    paste_expiry_sec: u64,
    #[arg(long = "no-cleanup", default_value_t = false)]
    no_clean_pastedir_on_start: bool,
}

type SafeGen = Arc<Mutex<PartitionIdGenerator>>;

const CLEANUP_WORKER_TAG: &str = "🧹";

const SOCKET_FILENAME: &str = "note.sock";

const PASTE_ID_MIN_LEN: usize = 5;

// regexp should describe set of PASTE_ID_SYMBOLS
const PASTE_ID_REGEXP: &str = "[a-z0-9]";

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
    let paste_limit = args.paste_len_kb * 1000;
    let paste_dir = Path::new(&args.paste_dir);
    let paste_timeout = Duration::from_secs(args.paste_expiry_sec);

    // +1 to catch pastes that violate the limit
    let mut buf = vec![0; paste_limit + 1];

    let shutdown = |stream: &mut Socket, mode: Shutdown, log_error_as: Option<log::Level>| {
        if mode == Shutdown::Write || mode == Shutdown::Both {
            stream.flush().ok();
        }
        stream
            .shutdown(mode)
            .map_err(|why| {
                if let Some(level) = log_error_as {
                    log::log!(level, "{} | {:?}: {}", tag, mode, why)
                }
            })
            .ok()
    };

    'outer: loop {
        let mut stream = match rx_paste.recv() {
            Ok(stream) => stream,
            Err(why) => {
                error!("{} | rx.recv: {}", tag, why);
                continue;
            }
        };

        stream
            .set_read_timeout(Some(Duration::from_millis(args.read_timeout)))
            .map_err(|why| warn!("{} | set_read_timeout: {}", tag, why))
            .ok();
        stream
            .set_write_timeout(Some(Duration::from_millis(args.read_timeout)))
            .map_err(|why| warn!("{} | set_write_timeout: {}", tag, why))
            .ok();

        // this is necessary if the buffer is preallocated because
        //  1. stream.read() will only read up to buf.len()
        //  2. buf.truncate(newlen) reduces buf.len(),
        //     which reduces it's effective size for stream.read()
        buf.resize(paste_limit + 1, 0);

        let mut read = 0;

        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    read += n;

                    if read > paste_limit {
                        stream
                            .write_all(
                                format!("Exceeded limit of {}kB\n", args.paste_len_kb).as_bytes(),
                            )
                            .map_err(|why| {
                                warn!("{} | stream.write_all on filesize limit: {}", tag, why)
                            })
                            .ok();

                        shutdown(&mut stream, Shutdown::Both, Some(Level::Warn));

                        continue 'outer;
                    }
                }
                Err(why) if why.kind() == ErrorKind::Interrupted => continue,
                Err(why) => {
                    warn!("{} | stream.read: {}", tag, why);

                    continue 'outer;
                }
            }
        }

        shutdown(&mut stream, Shutdown::Read, Some(Level::Warn));

        buf.truncate(read);
        match std::str::from_utf8(&buf) {
            Err(_) => {
                stream
                    .write_all(b"invalid utf-8\n")
                    .map_err(|why| warn!("{} | stream.write_all on invalid utf-8: {}", tag, why))
                    .ok();
            }
            Ok(payload) => {
                let mut gen = gen.lock().expect("Some thread has crashed!");

                let paste_id = match gen.get() {
                    Some(id) => id,
                    None => {
                        // no ID can be generated, "adress space is full"
                        stream
                            .write_all(
                                b"server is currently not accepting new pastes. try again later.\n",
                            )
                            .map_err(|why| {
                                warn!("{} | stream.write_all on unavailable ids: {}", tag, why)
                            })
                            .ok();
                        shutdown(&mut stream, Shutdown::Both, Some(Level::Warn));
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
                        continue;
                    }
                }

                drop(gen);

                let expiry_string = match args.paste_expiry_sec {
                    x if x > 60 => match x % 60 {
                        y if y > 0 => format!("{}m {}s", x / 60, y),
                        _ => format!("{}m", x / 60),
                    },
                    x => format!("{}s", x),
                };

                stream
                    .write_all(
                        format!(
                            "{}/{} | expires in {}\n",
                            args.host, paste_id, expiry_string
                        )
                        .as_bytes(),
                    )
                    .map_err(|why| error!("{} | stream.write_all on success reply: {}", tag, why))
                    .ok();
            }
        }

        shutdown(&mut stream, Shutdown::Write, Some(Level::Warn));
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
        regex::Regex::new(&format!("{}{{{},}}", PASTE_ID_REGEXP, PASTE_ID_MIN_LEN))
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
    socket
        .set_nonblocking(false)
        .expect("Could not set socket to blocking");
    fs::set_permissions(&socket_path, Permissions::from_mode(args.socket_mode))
        .expect("Could not set socket permission");
    socket
        .listen(args.workers as i32 * 2)
        .expect("Could not start listening");

    CombinedLogger::init(vec![TermLogger::new(
        LevelFilter::Trace,
        Config::default(),
        TerminalMode::Stdout,
        ColorChoice::Auto,
    )])
    .unwrap();

    if let Some(ref set) = id_set {
        if !args.no_clean_pastedir_on_start {
            for f in set.iter() {
                fs::remove_dir_all(paste_path.join(f))
                    .map(|()| info!("Cleaned up old '{:?}'", f))
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
        PartitionIdGenerator::new("1111", "zzzz", true, 1024, id_set)
            .expect("Could not create id generator"),
    ));

    info!(
        "Starting notesock v{} on <b>{}</b> 🧦",
        CARGO_VERSION,
        socket_path
            .canonicalize()
            .expect("Bad socket path")
            .display()
    );

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
