//! Machines and the Mac.
//!
//! - `mac <command>` in a machine runs the command on the Mac, as the Mac
//!   user, in the same folder when it's under the shared home. The machine's
//!   `/usr/local/bin/mac` is a link to this binary (client mode), which talks
//!   to `/run/berth/mac.sock`.
//! - The Mac's SSH agent is at `/run/berth/ssh-agent.sock` in machines
//!   (`SSH_AUTH_SOCK`), and at `/run/host-services/ssh-auth.sock` in the VM
//!   for containers that mount it, the path Docker Desktop uses.
//! - `ssh <machine>.berth` on the Mac reaches the machine's sshd through the
//!   `machine-ssh` header (machines.rs installs sshd).
//!
//! The sockets exist only in the VM and, bind-mounted, in machines, not in
//! containers. Each request to the Mac also carries a token the host hands
//! us (`host-token`): anything in the VM can reach the host's vsock port, so
//! without it a container could run commands on the Mac.

use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::hostnet::{HOST_VSOCK_PORT, VMADDR_CID_HOST, vsock_connect};

static TOKEN: Mutex<Option<String>> = Mutex::new(None);

pub const DIR: &str = "/run/berth";
const MAC_SOCK: &str = "/run/berth/mac.sock";
const AGENT_SOCK: &str = "/run/berth/ssh-agent.sock";
const DOCKER_AGENT_SOCK: &str = "/run/host-services/ssh-auth.sock";

pub fn set_token(token: &str) {
    if token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
        *TOKEN.lock().unwrap() = Some(token.to_string());
    }
}

/// Listen on the sockets machines (and the VM) use to reach the Mac.
pub fn run() {
    for (path, kind) in [(MAC_SOCK, "mac-exec"), (AGENT_SOCK, "ssh-agent"), (DOCKER_AGENT_SOCK, "ssh-agent")] {
        match listen(path) {
            Ok(listener) => {
                std::thread::spawn(move || {
                    for conn in listener.incoming() {
                        let Ok(conn) = conn else {
                            // Out of descriptors, typically: don't spin.
                            std::thread::sleep(std::time::Duration::from_millis(100));
                            continue;
                        };
                        std::thread::spawn(move || {
                            if let Err(err) = relay(conn, kind) {
                                eprintln!("berth-agent: {kind}: {err}");
                            }
                        });
                    }
                });
            }
            Err(err) => eprintln!("berth-agent: {path}: {err}"),
        }
    }
}

fn listen(path: &str) -> io::Result<UnixListener> {
    let dir = std::path::Path::new(path).parent().unwrap_or(std::path::Path::new("/"));
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755))?;
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    // Any user in a machine may use them, as with `ssh -A`.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))?;
    Ok(listener)
}

/// Pass a connection on to the Mac with the token in front.
fn relay(conn: UnixStream, kind: &str) -> io::Result<()> {
    let Some(token) = TOKEN.lock().unwrap().clone() else {
        return Err(io::Error::other("not connected to the Mac yet"));
    };
    let mut host = UnixStream::from(vsock_connect(VMADDR_CID_HOST, HOST_VSOCK_PORT)?);
    writeln!(host, "{kind} {token}")?;
    crate::splice(conn, host)
}

/// `machine-ssh <name>`: connect to the machine's sshd, starting the machine
/// if it's stopped (VS Code connecting brings it up).
pub fn machine_ssh(name: &str, host: UnixStream) -> io::Result<()> {
    let address = || crate::machines::running_addresses().into_iter().find(|(n, _)| n == name).map(|(_, ip)| ip);
    let mut ip = address();
    if ip.is_none() {
        crate::machines::start(name)?;
        for _ in 0..60 {
            std::thread::sleep(std::time::Duration::from_millis(500));
            ip = address();
            if ip.is_some() {
                break;
            }
        }
    }
    let ip = ip.ok_or_else(|| io::Error::other(format!("machine {name} has no address")))?;
    let ssh = std::net::SocketAddr::from((ip, 22));
    let connect = || std::net::TcpStream::connect_timeout(&ssh, std::time::Duration::from_secs(3));
    // A machine made before this has no sshd: set it up on first use.
    if connect().is_err()
        && let Err(err) = crate::machines::ensure_sshd(name)
    {
        eprintln!("berth-agent: machines: sshd in {name}: {err}");
    }
    // sshd may still be starting just after boot.
    let mut last = None;
    for _ in 0..20 {
        match connect() {
            Ok(sshd) => return crate::splice(host, sshd),
            Err(err) => last = Some(err),
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Err(last.unwrap_or_else(|| io::Error::other("no sshd")))
}

/// What `mac` asks the Mac to run (src/macexec.rs on the host).
#[derive(Serialize, Deserialize)]
pub struct Request {
    pub args: Vec<String>,
    pub cwd: String,
    pub tty: bool,
    pub rows: u16,
    pub cols: u16,
    pub term: String,
}

/// Frames from the Mac: 1 stdout, 2 stderr, 3 the exit code (4 bytes).
const STDOUT: u8 = 1;
const STDERR: u8 = 2;
const EXIT: u8 = 3;

/// The `mac` command (this binary run as `mac`). Returns the exit code.
pub fn client(args: Vec<String>) -> i32 {
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        eprintln!("usage: mac <command> [args...]   run a command on the Mac");
        return 2;
    }
    match client_run(args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("mac: {err}");
            255
        }
    }
}

fn client_run(args: Vec<String>) -> io::Result<i32> {
    let mut conn = UnixStream::connect(MAC_SOCK)
        .map_err(|e| io::Error::other(format!("{MAC_SOCK}: {e} (is this a berth machine?)")))?;
    let tty = unsafe { libc::isatty(0) == 1 && libc::isatty(1) == 1 };
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    if tty {
        unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) };
    }
    let request = Request {
        args,
        cwd: std::env::current_dir().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default(),
        tty,
        rows: size.ws_row,
        cols: size.ws_col,
        term: std::env::var("TERM").unwrap_or_default(),
    };
    let mut line = serde_json::to_vec(&request).map_err(io::Error::other)?;
    line.push(b'\n');
    conn.write_all(&line)?;

    let _raw = if tty { RawMode::enter() } else { None };
    let mut to_mac = conn.try_clone()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        let stdin = io::stdin();
        let mut stdin = stdin.lock();
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if to_mac.write_all(&buf[..n]).is_err() {
                        return;
                    }
                }
            }
        }
        let _ = to_mac.shutdown(std::net::Shutdown::Write);
    });
    let (mut stdout, mut stderr) = (io::stdout(), io::stderr());
    loop {
        let mut head = [0u8; 5];
        if conn.read_exact(&mut head).is_err() {
            return Err(io::Error::other("lost the connection to the Mac"));
        }
        let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        // The Mac sends at most 64 KiB; more means we lost the framing.
        if len > 1 << 20 {
            return Err(io::Error::other("bad frame from the Mac"));
        }
        let mut body = vec![0u8; len];
        conn.read_exact(&mut body)?;
        match head[0] {
            STDOUT => {
                stdout.write_all(&body)?;
                stdout.flush()?;
            }
            STDERR => {
                stderr.write_all(&body)?;
                stderr.flush()?;
            }
            EXIT if body.len() == 4 => return Ok(i32::from_be_bytes([body[0], body[1], body[2], body[3]])),
            _ => return Err(io::Error::other("bad frame from the Mac")),
        }
    }
}

/// The terminal in raw mode while a command on the Mac owns it.
struct RawMode(libc::termios);

impl RawMode {
    fn enter() -> Option<Self> {
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut saved) } != 0 {
            return None;
        }
        let mut raw = saved;
        unsafe {
            libc::cfmakeraw(&mut raw);
            libc::tcsetattr(0, libc::TCSANOW, &raw);
        }
        Some(RawMode(saved))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
    }
}
