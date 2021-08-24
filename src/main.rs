extern crate clap;
extern crate daemonize;
extern crate env_logger;
extern crate libc;
extern crate signal_hook;
#[macro_use]
extern crate log;
extern crate net2;
extern crate num_cpus;
extern crate time;

use net2::{TcpBuilder, UdpBuilder};
use signal_hook::consts::signal::{SIGINT, SIGTERM};
use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::thread;

#[cfg(target_os = "linux")]
const SO_REUSEPORT: i32 = libc::SO_REUSEPORT;

#[cfg(target_os = "freebsd")]
const SO_REUSEPORT: i32 = 0x00010000; /* Use SO_REUSEPORT_LB */

const BACKLOG: i32 = 1024;
const PACKETS_PER_CON: &'static str = "100";

#[derive(PartialEq, Copy, Clone, Debug)]
enum Protocol {
    TCP,
    UDP,
}

fn main() {
    let matches = clap::App::new("pperf")
        .version("0.1.0")
        .author("Johannes Lundberg <johalun0@gmail.com>")
        .about("Iperf-like performance utility")
        .arg(
            clap::Arg::with_name("server")
                .short("s")
                .long("server")
                .required(false)
                .takes_value(false)
                .help("Run as server"),
        )
        .arg(
            clap::Arg::with_name("server ip")
                .short("c")
                .long("client")
                .required(false)
                .takes_value(true)
                .help("Run as client, connect to <server ipv4> (can be a range)"),
        )
        .arg(
            clap::Arg::with_name("port")
                .short("p")
                .long("port")
                .required(false)
                .takes_value(true)
                .default_value("5001")
                .help("Port to listen/connect to"),
        )
        .arg(
            clap::Arg::with_name("packets")
                .short("n")
                .long("packets")
                .required(false)
                .takes_value(true)
                .default_value(PACKETS_PER_CON)
                .help("Send n packets per connection"),
        )
        .arg(
            clap::Arg::with_name("threads")
                .short("t")
                .long("threads")
                .required(false)
                .takes_value(true)
                .help("Number of threads (default: number of CPUs)"),
        )
        .arg(
            clap::Arg::with_name("debug")
                .short("d")
                .long("debug")
                .help("Enable debug output"),
        )
        .arg(
            clap::Arg::with_name("protocol")
                .short("u")
                .long("udp")
                .help("Use UDP instead of TCP"),
        )
        .arg(
            clap::Arg::with_name("background")
                .short("b")
                .long("background")
                .takes_value(false)
                .help("Run in background"),
        )
        .arg(
            clap::Arg::with_name("rates")
                .short("r")
                .long("rates")
                .takes_value(false)
                .help("Print rate info each second"),
        )
        .get_matches();

    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG_STYLE", "never");
        if matches.is_present("debug") {
            std::env::set_var("RUST_LOG", "debug");
        } else {
            std::env::set_var("RUST_LOG", "info");
        }
    }
    env_logger::Builder::from_default_env()
        .format_timestamp(None)
        .init();
    debug!("Enabled debug output.");

    let server = matches.is_present("server");
    let client = matches.is_present("server ip");
    if (server && client) || (!server && !client) {
        error!("Choose either server or client.");
        std::process::exit(1);
    }

    //
    // Common stuff
    //
    let (signal_send, signal_recv) = channel();
    let mut signals = signal_hook::iterator::Signals::new([SIGINT, SIGTERM])
        .expect("Error creating signal hook");
    thread::spawn(move || {
        for signal in signals.forever() {
            debug!("Caught signal {} in signal handler.", signal);
            if signal_send.send(signal).is_err() {
                break;
            }
        }
    });

    let port: usize = matches
        .value_of("port")
        .unwrap()
        .parse::<usize>()
        .unwrap_or_else(|e| {
            error!("Port not valid number: {}", e);
            std::process::exit(1);
        });

    let ncpus: usize = matches
        .value_of("threads")
        .unwrap_or(&num_cpus::get().to_string())
        .parse::<usize>()
        .unwrap_or_else(|e| {
            error!("Threads not valid number: {}", e);
            std::process::exit(1);
        });
    info!("Running {} threads.", ncpus);

    let protocol = match matches.is_present("protocol") {
        true => Protocol::UDP,
        false => Protocol::TCP,
    };

    let background = matches.is_present("background");
    if background {
        info!("Daemonizing...");
        let stdout =
            std::fs::File::create("/tmp/pperf.out").expect("Error creating /tmp/pperf.out");
        let stderr =
            std::fs::File::create("/tmp/pperf.err").expect("Error creating /tmp/pperf.err");

        let daemonize = daemonize::Daemonize::new().stdout(stdout).stderr(stderr);

        match daemonize.start() {
            Ok(_) => info!("Daemonized"),
            Err(e) => error!("{}", e),
        }
    }

    //
    // Run as server
    //
    if server && !client {
        let address = format!("0.0.0.0:{}", port);
        info!("Starting {:?} server. Listening on {}.", protocol, address);

        let mut packet_counts = vec![];
        for _ in 0..ncpus {
            packet_counts.push(Arc::new(AtomicUsize::new(0)));
        }
        let mut threadhandlers = vec![];
        for i in 0..ncpus {
            let addr = address.clone();
            let pc: Arc<AtomicUsize> = packet_counts[i].clone();
            let thread = std::thread::spawn(move || {
                //
                // UDP
                //
                let mut buf = [0; 4];
                if protocol == Protocol::UDP {
                    let builder = UdpBuilder::new_v4().expect("UDP new_v4");
                    set_sockopt(builder.as_raw_fd(), SO_REUSEPORT).expect("set_sockopt");
                    let socket = builder.bind(addr).expect("bind");
                    loop {
                        let (_len, src) = socket.recv_from(&mut buf).expect("recv_from");
                        debug!("{:02}: Got {:?} from {}", i, std::str::from_utf8(&buf), src);
                        pc.fetch_add(1, Ordering::SeqCst);
                    }
                }
                //
                // TCP
                //
                else if protocol == Protocol::TCP {
                    let builder = TcpBuilder::new_v4().expect("TCP new_v4");
                    set_sockopt(builder.as_raw_fd(), SO_REUSEPORT).expect("set_sockopt");
                    let socket = builder.bind(addr).expect("bind");
                    let listener: TcpListener = socket.listen(BACKLOG).expect("listen");
                    for stream in listener.incoming() {
                        let pc = pc.clone();
                        std::thread::spawn(move || {
                            debug!("{:02}: Got connection: {:?}", i, stream);
                            match stream {
                                Ok(mut stream) => loop {
                                    let mut buf = [0u8; 4];
                                    if let Ok(_) = stream.read_exact(&mut buf) {
                                        let msg = std::str::from_utf8(&buf)
                                            .expect("Error parsing message");
                                        debug!("{:02}: Got message: {:?}, replying...", i, msg);
                                        if msg == "PING" {
                                            let _ = stream
                                                .write_all("PONG".as_bytes())
                                                .expect("Server stream write");
                                        } else if msg == "FINI" {
                                            debug!("{:02}: Client closed the connection", i);
                                            break;
                                        }
                                        pc.fetch_add(1, Ordering::SeqCst);
                                    } else {
                                        error!("{:02}: Reading from client", i);
                                        break;
                                    }
                                },
                                Err(e) => error!("Stream error: {}", e),
                            }
                        });
                    }
                }
            });
            threadhandlers.push(thread);
        }

        if matches.is_present("rates") {
            let thread = std::thread::spawn(move || {
                let mut values = vec![0usize; ncpus];
                let mut sum = 0usize;
                loop {
                    for i in 0..ncpus {
                        let new_value = packet_counts[i].load(Ordering::Relaxed);
                        let diff = new_value - values[i];
                        print!("[{:?}] ", diff);
                        sum += diff;
                        values[i] = new_value;
                    }
                    println!(" ({})", sum);
                    sum = 0;
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
            });
            threadhandlers.push(thread);
        }

        signal_recv.recv().unwrap();
        info!("Received signal, exiting...");
    }

    //
    // Run as client
    //
    if client && !server {
        let mut packet_counts = vec![];
        for _ in 0..ncpus {
            packet_counts.push(Arc::new(AtomicUsize::new(0)));
        }
        let packets_send_count: usize = matches
            .value_of("packets")
            .unwrap()
            .parse::<usize>()
            .unwrap_or_else(|e| {
                error!("Packets not valid number: {}", e);
                std::process::exit(1);
            });
        let server_ip: &str = matches
            .value_of("server ip")
            .expect("Error getting server IP from args.");

        let mut addresses = vec![];
        if server_ip.contains('-') {
            let range_vec: Vec<&str> = server_ip.split('-').collect();
            let addr_vec: Vec<&str> = range_vec[0].split('.').collect();
            let base: u8 = addr_vec[3]
                .parse::<u8>()
                .expect("Error parsing server ip range start to u8");
            let end: u8 = range_vec[1]
                .parse::<u8>()
                .expect("Error parsing server ip range end to u8");
            assert!(base < end);
            for i in base..end + 1 {
                addresses.push(format!(
                    "{}.{}.{}.{}:{}",
                    addr_vec[0], addr_vec[1], addr_vec[2], i, port
                ));
            }
        } else {
            addresses.push(format!("{}:{}", server_ip, port));
        }
        let addresses = addresses;

        let running = Arc::new(AtomicBool::new(true));
        let packets_sent_total = Arc::new(AtomicUsize::new(0));
        let connections = Arc::new(AtomicUsize::new(0));
        let time_start = time::precise_time_s();

        info!("Starting {:?} connections to: {:?}", protocol, addresses);
        info!("Sending {} packets per connection.", packets_send_count);

        let mut threadhandlers = vec![];
        for tid in 0..ncpus {
            let pc: Arc<AtomicUsize> = packet_counts[tid].clone();
            let addr_vec = addresses.clone();
            let running = running.clone();
            let packets_sent_total = packets_sent_total.clone();
            let connections = connections.clone();
            let thread = std::thread::spawn(move || {
                let mut iter = 0;
                while running.load(Ordering::SeqCst) {
                    let mut sent = 0;
                    let addr = &addr_vec[(iter + tid) % addr_vec.len()];
                    connections.fetch_add(1, Ordering::SeqCst);
                    iter += 1;
                    debug!("{:02}: Connecting to {}", tid, addr);
                    //
                    // UDP
                    //
                    if protocol == Protocol::UDP {
                        let socket = UdpSocket::bind("0.0.0.0:0").expect("bind");
                        while running.load(Ordering::SeqCst) && sent < packets_send_count {
                            packets_sent_total.fetch_add(1, Ordering::SeqCst);
                            pc.fetch_add(1, Ordering::SeqCst);
                            sent += 1;
                            debug!("{:02}: Sending packet to {}", tid, addr);
                            socket.send_to("PING".as_bytes(), &addr).expect("send_to");
                        }
                    }
                    //
                    // TCP
                    //
                    else if protocol == Protocol::TCP {
                        let mut buf = [0u8; 4];
                        match TcpBuilder::new_v4().expect("new_v4").connect(addr.clone()) {
                            Ok(mut stream) => {
                                while running.load(Ordering::SeqCst) && sent < packets_send_count {
                                    pc.fetch_add(1, Ordering::SeqCst);
                                    packets_sent_total.fetch_add(1, Ordering::SeqCst);
                                    sent += 1;
                                    debug!("{:02}: Connected: {:?}", tid, stream);
                                    debug!("{:02}: Sending PING", tid);
                                    let _ = stream
                                        .write_all("PING".as_bytes())
                                        .expect("Client stream write");
                                    let _ =
                                        stream.read_exact(&mut buf).expect("Client stream read");
                                    debug!(
                                        "{:02}: Got reply: {:?}",
                                        tid,
                                        std::str::from_utf8(&buf)
                                    );
                                }
                                debug!("{:02}: Sending FINI", tid);
                                let _ = stream
                                    .write_all("FINI".as_bytes())
                                    .expect("Client stream write");
                            }
                            Err(e) => {
                                println!("Stream error: {:?}", e);
                                std::thread::sleep(std::time::Duration::from_secs(1));
                            }
                        }
                    }
                }
            });
            threadhandlers.push(thread);
        }

        if matches.is_present("rates") {
            let running = running.clone();
            let thread = std::thread::spawn(move || {
                let mut values = vec![0usize; ncpus];
                let mut sum = 0usize;
                while running.load(Ordering::SeqCst) {
                    print!("PPS: ");
                    for i in 0..ncpus {
                        let new_value = packet_counts[i].load(Ordering::Relaxed);
                        let diff = new_value - values[i];
                        print!("[{:?}] ", diff);
                        sum += diff;
                        values[i] = new_value;
                    }
                    println!(" ({})", sum);
                    sum = 0;
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
            });
            threadhandlers.push(thread);
        }

        signal_recv.recv().unwrap();
        info!("Received signal");
        running.store(false, Ordering::SeqCst);
        for thread in threadhandlers {
            let _ = thread.join();
        }
        let time_used = time::precise_time_s() - time_start;

        info!(
            "Averaged {} packets per second with {} connections per second",
            packets_sent_total.load(Ordering::SeqCst) / time_used as usize,
            connections.load(Ordering::SeqCst) / time_used as usize
        );
    }
}

fn set_sockopt(fd: i32, opt: i32) -> Result<(), ()> {
    let optval: libc::c_int = 1;
    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            opt,
            &optval as *const _ as *const libc::c_void,
            std::mem::size_of_val(&optval) as libc::socklen_t,
        )
    };
    if ret == 0 {
        return Ok(());
    }
    return Err(());
}
