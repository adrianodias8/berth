//! Running GUI-session commands inside a machine, as the machine's user.
//!
//! The same two ways in as machines.rs's `run_in`: through the machine's own
//! systemd (`systemd-run -M`, so the process has the machine's cgroup,
//! devices and seccomp) when PID 1 is systemd, else nsenter + su. Long-running
//! processes (compositors, apps) get a transient unit of their own: a
//! `--wait` unit's leftover processes are killed when its main process exits.
//! Arguments go to Command as argv, so only the scripts themselves are shell.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
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

    /// `command`, as the machine's root: for what a session needs from the
    /// machine itself (seatd, the seat group).
    fn root_command(&self, script: &str) -> io::Result<Command> {
        let mut cmd;
        if self.systemd {
            cmd = Command::new("systemd-run");
            cmd.args(["-M", &self.name, "--wait", "--pipe", "--quiet", "--collect"]);
            cmd.args([&format!("--setenv=BERTH_SCRIPT={script}"), "/bin/sh", "-c", SCRIPT_FROM_ENV]);
        } else {
            let leader = sh_out(Command::new("machinectl").args(["show", &self.name, "-p", "Leader", "--value"]))?;
            cmd = Command::new("nsenter");
            cmd.args(["-t", leader.trim(), "-a", "--", "/bin/sh", "-c", script]);
        }
        Ok(cmd)
    }

    /// `run`, as the machine's root.
    pub fn run_root(&self, script: &str, timeout: Duration) -> io::Result<String> {
        run_with_timeout(self.root_command(script)?, &[], timeout).map(|out| String::from_utf8_lossy(&out).into_owned())
    }

    /// `spawn`, as the machine's root, for a daemon the machine keeps
    /// (seatd): a unit that's already running is left as it is.
    pub fn spawn_root(&self, unit: &str, script: &str) -> io::Result<()> {
        if self.systemd {
            let mut active = Command::new("systemctl");
            active.args(["-M", &self.name, "is-active", "--quiet", unit]);
            if run_with_timeout(active, &[], Duration::from_secs(10)).is_ok() {
                return Ok(());
            }
            let mut cmd = Command::new("systemd-run");
            cmd.args(["-M", &self.name, &format!("--unit={unit}"), "--quiet", "--collect"]);
            cmd.args([&format!("--setenv=BERTH_SCRIPT={script}"), "/bin/sh", "-c", SCRIPT_FROM_ENV]);
            return run_with_timeout(cmd, &[], Duration::from_secs(30)).map(|_| ());
        }
        let detached = format!("setsid /bin/sh -c {} </dev/null >/dev/null 2>&1 &", quote(script));
        self.run_root(&detached, Duration::from_secs(30)).map(|_| ())
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
