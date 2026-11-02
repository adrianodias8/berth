//! Running GUI-session commands inside a machine, as the machine's user.
//!
//! The same two ways in as machines.rs's `run_in`: through the machine's own
//! systemd (`systemd-run -M`, so the process has the machine's cgroup,
//! devices and seccomp) when PID 1 is systemd, else nsenter + su. Long-running
//! processes (compositors, apps) get a transient unit of their own: a
//! `--wait` unit's leftover processes are killed when its main process exits.
//! Arguments go to Command as argv, so only the scripts themselves are shell.

use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::machines;

/// systemd expands `${…}` in a unit's command line, which would eat the
/// scripts' own; so the script travels in the environment, and the command
/// line names it the one way systemd passes through as a single word.
const SCRIPT_FROM_ENV: &str = "${BERTH_SCRIPT}";

pub struct Machine {
    pub name: String,
    pub user: String,
    systemd: bool,
}

impl Machine {
    /// A running machine and its berth user (the one `machine create` made).
    pub fn open(name: &str) -> io::Result<Machine> {
        if !machines::valid_name(name) || !std::path::Path::new(&format!("/var/lib/machines/{name}")).is_dir() {
            return Err(io::Error::other(format!("no machine {name}")));
        }
        if !machines::running(name) {
            return Err(io::Error::other(format!("machine {name} isn't running: berth machine start {name}")));
        }
        let user = machines::machine_user(name)?;
        let systemd = machines::runs_systemd(name);
        if !systemd && machines::isolated(name) {
            return Err(io::Error::other("an isolated machine without systemd: berth won't run anything in it"));
        }
        Ok(Machine { name: name.to_string(), user, systemd })
    }

    fn command(&self, script: &str) -> io::Result<Command> {
        let mut cmd;
        if self.systemd {
            cmd = Command::new("systemd-run");
            cmd.args(["-M", &self.name, &format!("--uid={}", self.user), "--wait", "--pipe", "--quiet", "--collect"]);
            cmd.args(["-p", "WorkingDirectory=~", &format!("--setenv=BERTH_SCRIPT={script}"), "/bin/sh", "-c", SCRIPT_FROM_ENV]);
        } else {
            let leader = sh_out(Command::new("machinectl").args(["show", &self.name, "-p", "Leader", "--value"]))?;
            cmd = Command::new("nsenter");
            cmd.args(["-t", leader.trim(), "-a", "--", "su", "-s", "/bin/sh", &self.user, "-c", script]);
        }
        Ok(cmd)
    }

    /// Run `script` with `stdin`, returning its stdout; an error carries the
    /// last lines of its stderr.
    pub fn run(&self, script: &str, stdin: &[u8], timeout: Duration) -> io::Result<Vec<u8>> {
        run_with_timeout(self.command(script)?, stdin, timeout)
    }

    /// `run` for text output.
    pub fn run_text(&self, script: &str, timeout: Duration) -> io::Result<String> {
        self.run(script, &[], timeout).map(|out| String::from_utf8_lossy(&out).into_owned())
    }

    /// Start `script` in the background under `unit` (a name of our own:
    /// berth-gui-<session>…), detached from this call.
    pub fn spawn(&self, unit: &str, script: &str) -> io::Result<()> {
        if self.systemd {
            let mut cmd = Command::new("systemd-run");
            cmd.args(["-M", &self.name, &format!("--uid={}", self.user), &format!("--unit={unit}"), "--quiet", "--collect"]);
            cmd.args(["-p", "WorkingDirectory=~", &format!("--setenv=BERTH_SCRIPT={script}"), "/bin/sh", "-c", SCRIPT_FROM_ENV]);
            return run_with_timeout(cmd, &[], Duration::from_secs(30)).map(|_| ());
        }
        let detached = format!("setsid /bin/sh -c {} </dev/null >/dev/null 2>&1 &", quote(script));
        self.run(&detached, &[], Duration::from_secs(30)).map(|_| ())
    }

    /// Stop the transient units of a session (systemd machines; elsewhere
    /// the session's own script kills its processes).
    pub fn stop_units(&self, pattern: &str) {
        if self.systemd {
            let _ = run_with_timeout(
                {
                    let mut c = Command::new("systemctl");
                    c.args(["-M", &self.name, "stop", pattern]);
                    c
                },
                &[],
                Duration::from_secs(30),
            );
        }
    }
}

fn sh_out(cmd: &mut Command) -> io::Result<String> {
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(io::Error::other(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Run `cmd`, feeding it `stdin`; killed past `timeout`.
pub fn run_with_timeout(mut cmd: Command, stdin: &[u8], timeout: Duration) -> io::Result<Vec<u8>> {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
    let mut input = child.stdin.take().unwrap();
    let data = stdin.to_vec();
    let writer = std::thread::spawn(move || {
        let _ = input.write_all(&data);
    });
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err.read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(io::ErrorKind::TimedOut, format!("timed out after {}s", timeout.as_secs())));
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = writer.join();
    let stdout = reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if !status.success() {
        let text = String::from_utf8_lossy(&stderr);
        let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let msg = tail[tail.len().saturating_sub(3)..].join("; ");
        return Err(io::Error::other(if msg.is_empty() { format!("command failed ({status})") } else { msg }));
    }
    Ok(stdout)
}

/// One shell word.
pub fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// A process in the machine whose stdin and stdout are ours: a session's
/// stream (capture or input). Dropping it ends the process and its unit.
pub struct Piped {
    child: Child,
    stderr: Option<std::thread::JoinHandle<Vec<u8>>>,
    stop: Stopper,
}

/// Ends a `Piped`'s process from another thread.
#[derive(Clone)]
pub struct Stopper {
    machine: String,
    unit: Option<String>,
    pid: i32,
    /// Set once the process has been waited for: its pid (and group) may
    /// belong to someone else after that.
    done: Arc<Mutex<bool>>,
}

impl Stopper {
    pub fn stop(&self) {
        let done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        if *done {
            return;
        }
        if let Some(unit) = &self.unit {
            let mut stop = Command::new("systemctl");
            stop.args(["-M", &self.machine, "stop", unit]);
            let _ = run_with_timeout(stop, &[], Duration::from_secs(10));
        }
        // Its own process group: nsenter, su and the shell under it.
        unsafe { libc::kill(-self.pid, libc::SIGTERM) };
    }
}

impl Piped {
    pub fn stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }

    pub fn stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    pub fn stopper(&self) -> Stopper {
        self.stop.clone()
    }

    /// Wait for the process to end: its exit code (None when killed) and the
    /// last lines of its stderr.
    pub fn wait(&mut self) -> (Option<i32>, String) {
        // Reaped only under the lock, which isn't held while waiting: a
        // stop() meanwhile still finds the process.
        let status = loop {
            {
                let mut done = self.stop.done.lock().unwrap_or_else(|e| e.into_inner());
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        *done = true;
                        break Some(status);
                    }
                    Ok(None) => {}
                    Err(_) => {
                        *done = true;
                        break None;
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let stderr = self.stderr.take().and_then(|h| h.join().ok()).unwrap_or_default();
        let text = String::from_utf8_lossy(&stderr);
        let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        (status.and_then(|s| s.code()), tail[tail.len().saturating_sub(3)..].join("; "))
    }
}

impl Drop for Piped {
    fn drop(&mut self) {
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            self.stop.stop();
            let _ = self.child.kill();
        }
        self.wait();
    }
}

impl Machine {
    /// Start `script` under `unit` with its stdin and stdout piped to us,
    /// for as long as it runs: `systemd-run --pipe --wait` (the unit's
    /// stdio are our pipes, handed over as they are), or nsenter + su.
    pub fn spawn_piped(&self, unit: &str, script: &str) -> io::Result<Piped> {
        let mut cmd;
        if self.systemd {
            cmd = Command::new("systemd-run");
            cmd.args(["-M", &self.name, &format!("--uid={}", self.user), &format!("--unit={unit}"), "--pipe", "--quiet", "--wait", "--collect"]);
            cmd.args(["-p", "WorkingDirectory=~", &format!("--setenv=BERTH_SCRIPT={script}"), "/bin/sh", "-c", SCRIPT_FROM_ENV]);
        } else {
            cmd = self.command(script)?;
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
        let mut child = cmd.spawn()?;
        let mut err = child.stderr.take().unwrap();
        // Kept whole while small; past 64 KiB, only what came last.
        let stderr = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > 64 << 10 {
                    buf.drain(..buf.len() - (16 << 10));
                }
            }
            buf
        });
        let stop = Stopper { machine: self.name.clone(), unit: self.systemd.then(|| unit.to_string()), pid: child.id() as i32, done: Arc::new(Mutex::new(false)) };
        Ok(Piped { child, stderr: Some(stderr), stop })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_round_trips() {
        let word = "it's $HOME `x` \"y\"";
        let out = Command::new("/bin/sh").args(["-c", &format!("printf %s {}", quote(word))]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), word);
    }

    #[test]
    fn timeouts_and_errors() {
        let mut slow = Command::new("/bin/sh");
        slow.args(["-c", "sleep 5"]);
        assert_eq!(run_with_timeout(slow, &[], Duration::from_millis(200)).unwrap_err().kind(), io::ErrorKind::TimedOut);
        let mut failing = Command::new("/bin/sh");
        failing.args(["-c", "echo one >&2; echo two >&2; exit 3"]);
        assert_eq!(run_with_timeout(failing, &[], Duration::from_secs(5)).unwrap_err().to_string(), "one; two");
        let mut cat = Command::new("/bin/cat");
        cat.arg("-");
        assert_eq!(run_with_timeout(cat, b"abc", Duration::from_secs(5)).unwrap(), b"abc");
    }
}
