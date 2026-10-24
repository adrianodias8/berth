//! The Mac side of machines' integration (agent/src/mac.rs): running `mac`
//! commands, the SSH agent, and `ssh <machine>.berth`.
//!
//! Requests from the VM carry TOKEN, which only the agent gets (it accepts
//! the `host-token` header from the Mac only): anything in the VM can reach
//! our vsock port, containers included, and must not run commands here.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use crate::daemon::guest_stream;

/// Random per daemon, handed to the agent every few seconds (it may restart).
pub fn token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let mut bytes = [0u8; 32];
        unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    })
}

pub fn token_ok(given: &str) -> bool {
    // Constant time: the comparison isn't a timing oracle.
    let want = token().as_bytes();
    given.len() == want.len() && given.bytes().zip(want).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

pub async fn push_token() {
    if let Ok(mut guest) = guest_stream(&format!("host-token {}", token())).await {
        let _ = guest.read_to_end(&mut Vec::new()).await;
    }
}

/// `ssh-agent <token>`: the Mac's SSH agent.
pub async fn ssh_agent(mut guest: UnixStream) -> Result<()> {
    let path = agent_socket().context("no SSH agent on the Mac (SSH_AUTH_SOCK)")?;
    let mut agent = UnixStream::connect(&path).await.with_context(|| format!("SSH agent at {path}"))?;
    tokio::io::copy_bidirectional(&mut guest, &mut agent).await?;
    Ok(())
}

/// The daemon's SSH_AUTH_SOCK, or launchd's for the login session (the
/// app starts the daemon without a terminal's environment).
fn agent_socket() -> Option<String> {
    std::env::var("SSH_AUTH_SOCK").ok().filter(|s| !s.is_empty()).or_else(|| {
        let out = std::process::Command::new("launchctl").args(["getenv", "SSH_AUTH_SOCK"]).output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
    })
}

#[derive(Deserialize)]
struct Request {
    args: Vec<String>,
    cwd: String,
    tty: bool,
    rows: u16,
    cols: u16,
    term: String,
}

const STDOUT: u8 = 1;
const STDERR: u8 = 2;
const EXIT: u8 = 3;

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + body.len());
    out.push(kind);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// `mac-exec <token>`: a JSON request line, then the command's stdin from
/// the guest; we send back stdout/stderr frames and the exit code.
pub async fn exec(guest: UnixStream) -> Result<()> {
    let mut guest = BufReader::new(guest);
    let mut line = String::new();
    // Bounded: the request is a command line, not data.
    (&mut guest).take(256 * 1024).read_line(&mut line).await?;
    let req: Request = serde_json::from_str(&line).context("bad mac request")?;
    if req.args.is_empty() {
        bail!("empty command");
    }
    let home = crate::paths::Paths::new().home;
    // The shared home has the same path on both sides; elsewhere, home.
    let cwd = std::path::Path::new(&req.cwd);
    let cwd = if cwd.starts_with(&home) && cwd.is_dir() { cwd.to_path_buf() } else { home };
    // A login shell, for the user's PATH (Homebrew and the like).
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    // fish has no "$@": its arguments are $argv.
    let run = if shell.ends_with("/fish") { "exec $argv" } else { "exec \"$@\"" };
    let mut cmd = std::process::Command::new(&shell);
    cmd.args(["-l", "-c", run, "mac"]).args(&req.args).current_dir(cwd);
    if !req.term.is_empty() {
        cmd.env("TERM", &req.term);
    }

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    let (read_half, mut write_half) = tokio::io::split(guest);
    let writer = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if write_half.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = write_half.shutdown().await;
    });
    let code = if req.tty { run_tty(cmd, &req, read_half, tx.clone()).await? } else { run_pipes(cmd, read_half, tx.clone()).await? };
    let _ = tx.send(frame(EXIT, &code.to_be_bytes())).await;
    drop(tx);
    let _ = writer.await;
    Ok(())
}

type GuestRead = tokio::io::ReadHalf<BufReader<UnixStream>>;

async fn run_pipes(cmd: std::process::Command, mut input: GuestRead, tx: mpsc::Sender<Vec<u8>>) -> Result<i32> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::from(cmd);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let mut stdin = child.stdin.take().context("stdin")?;
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut input, &mut stdin).await;
        // Dropping stdin closes it: the command sees EOF.
    });
    let pump = |mut from: Box<dyn tokio::io::AsyncRead + Unpin + Send>, kind: u8, tx: mpsc::Sender<Vec<u8>>| {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n) = from.read(&mut buf).await {
                if n == 0 || tx.send(frame(kind, &buf[..n])).await.is_err() {
                    break;
                }
            }
        })
    };
    let out = pump(Box::new(child.stdout.take().context("stdout")?), STDOUT, tx.clone());
    let err = pump(Box::new(child.stderr.take().context("stderr")?), STDERR, tx);
    let status = child.wait().await?;
    let _ = out.await;
    let _ = err.await;
    Ok(exit_code(status))
}

async fn run_tty(mut cmd: std::process::Command, req: &Request, mut input: GuestRead, tx: mpsc::Sender<Vec<u8>>) -> Result<i32> {
    use std::os::unix::process::CommandExt;
    let size = libc::winsize { ws_row: req.rows.max(1), ws_col: req.cols.max(1), ws_xpixel: 0, ws_ypixel: 0 };
    let (mut master, mut slave) = (-1, -1);
    let mut size = size;
    if unsafe { libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), &mut size) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    // openpty's fds aren't close-on-exec: everything else the daemon starts
    // would inherit this session's pty and keep it from ever hanging up.
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    cmd.stdin(slave.try_clone()?).stdout(slave.try_clone()?).stderr(slave.try_clone()?);
    // Its own session with the pty as controlling terminal, so ^C and job
    // control reach the command.
    unsafe {
        cmd.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = tokio::process::Command::from(cmd).kill_on_drop(true).spawn()?;
    let pid = child.id().unwrap_or(0) as libc::pid_t;
    let exited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let still_running = {
        let exited = exited.clone();
        move || pid > 0 && !exited.load(std::sync::atomic::Ordering::SeqCst)
    };
    drop(slave);
    let master = std::fs::File::from(master);
    // pty writes block too: the guest's input goes to a writer thread.
    let (keys, mut typed) = mpsc::channel::<Vec<u8>>(16);
    let mut writer = master.try_clone()?;
    std::thread::spawn(move || {
        use std::io::Write;
        while let Some(bytes) = typed.blocking_recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        while let Ok(n) = input.read(&mut buf).await {
            if n == 0 || keys.send(buf[..n].to_vec()).await.is_err() {
                break;
            }
        }
        // A terminal sends no EOF: the `mac` side went away. Hang up, as a
        // closed terminal would; kill whatever ignores that.
        if still_running() {
            unsafe { libc::kill(-pid, libc::SIGHUP) };
            tokio::time::sleep(Duration::from_secs(3)).await;
            if still_running() {
                unsafe { libc::kill(-pid, libc::SIGKILL) };
            }
        }
    });
    // pty reads block; a thread keeps them off the async workers.
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut master = master;
        let mut buf = vec![0u8; 64 * 1024];
        // EIO once the command (and everything holding the pty) is gone.
        while let Ok(n) = master.read(&mut buf) {
            if n == 0 || tx.blocking_send(frame(STDOUT, &buf[..n])).is_err() {
                break;
            }
        }
    });
    let status = child.wait().await?;
    exited.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = tokio::task::spawn_blocking(move || reader.join()).await;
    Ok(exit_code(status))
}

fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// `~/.berth/run/machine-ssh.sock`: a machine name line, then the SSH
/// connection to that machine's sshd (`berth machine proxy`, the
/// ProxyCommand for `<machine>.berth`).
pub fn serve_machine_ssh(listener: tokio::net::UnixListener) {
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            tokio::spawn(async move {
                let result = async {
                    let mut client = BufReader::new(client);
                    let mut name = String::new();
                    (&mut client).take(64).read_line(&mut name).await?;
                    let name = name.trim();
                    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
                        bail!("bad machine name {name:?}");
                    }
                    let mut guest = guest_stream(&format!("machine-ssh {name}")).await?;
                    // Bytes the client sent after the name are in the buffer.
                    let buffered = client.buffer().to_vec();
                    guest.write_all(&buffered).await?;
                    let mut client = client.into_inner();
                    tokio::io::copy_bidirectional(&mut client, &mut guest).await?;
                    anyhow::Ok(())
                };
                if let Err(err) = result.await
                    && !crate::daemon::is_disconnect(&err)
                {
                    eprintln!("machine ssh: {err:#}");
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_our_token() {
        assert_eq!(token().len(), 64);
        assert!(token_ok(token()));
        assert!(!token_ok(""));
        assert!(!token_ok(&"0".repeat(64)));
        assert!(!token_ok(&token()[..63]));
    }

    #[test]
    fn frames() {
        assert_eq!(frame(EXIT, &7i32.to_be_bytes()), [3, 0, 0, 0, 4, 0, 0, 0, 7]);
        assert_eq!(frame(STDOUT, b"hi"), [1, 0, 0, 0, 2, b'h', b'i']);
    }
}
