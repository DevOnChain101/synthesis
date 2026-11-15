//! Tiny network probe the sandbox tests run INSIDE the sandbox (instead of python3, which on macOS
//! is an xcrun shim that dies under Seatbelt). Exit 0 = the operation worked, 1 = it failed.
//!   net_probe unix <socket-path>   connect to a unix socket
//!   net_probe connect <addr>       TCP connect to <addr> (e.g. 127.0.0.1:4000); exit 0 = connected
//!   net_probe loopback             bind/listen/connect/accept on 127.0.0.1 and ::1, then probe two
//!                                  non-loopback addresses; prints LOOPBACK_OK, remote_denied/REMOTE_CONNECTED
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::ExitCode;
use std::time::Duration;

fn pair(addr: &str) -> std::io::Result<()> {
    let l = TcpListener::bind((addr, 0))?;
    let c = TcpStream::connect_timeout(&l.local_addr()?, Duration::from_secs(2))?;
    l.accept()?;
    drop(c);
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        #[cfg(unix)]
        ["unix", path] => match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("unix connect {path}: {e}");
                ExitCode::FAILURE
            }
        },
        ["connect", addr] => match addr
            .parse::<SocketAddr>()
            .map_err(|e| e.to_string())
            .and_then(|a| {
                TcpStream::connect_timeout(&a, Duration::from_secs(3)).map_err(|e| e.to_string())
            }) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("connect {addr}: {e}");
                ExitCode::FAILURE
            }
        },
        ["loopback"] => {
            for a in ["127.0.0.1", "::1"] {
                if let Err(e) = pair(a) {
                    eprintln!("loopback {a}: {e}");
                    return ExitCode::FAILURE;
                }
            }
            println!("LOOPBACK_OK");
            for host in ["169.254.1.1:80", "192.0.2.1:80"] {
                let sa: SocketAddr = host.parse().unwrap();
                match TcpStream::connect_timeout(&sa, Duration::from_secs(2)) {
                    Ok(_) => println!("REMOTE_CONNECTED"),
                    Err(e) => println!("remote_denied {:?}", e.raw_os_error()),
                }
            }
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: net_probe unix <path> | connect <addr> | loopback");
            ExitCode::from(2)
        }
    }
}
