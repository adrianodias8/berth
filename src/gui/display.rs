//! The daemon's `display` stream (docs/display-protocol.md): a live view
//! of a surface for Berth.app and `berth gui watch`, and input back.
//!
//! A viewer sends `{"cmd":"display", …}` on the control socket and gets a
//! reply line naming the surface's ring (frames.rs, ring.rs), which it maps;
//! from then on the daemon sends doorbell lines (a frame is ready in slot
//! n, the size changed, the cursor moved), paced to the viewer's max_fps,
//! and the viewer may send input lines, forwarded to the guest while it has
//! taken control.
//!
//! A GUI session's surface is fed by the agent's `screen.stream` (one
//! feeder per session however many watch it, stopped 10 s after the last
//! viewer leaves); a scanout's by the GPU device, with no feeder. Input
//! goes to one `input.stream` per session, held while any viewer has
//! control. Both streams are docs/gui-protocol.md's exception to one
//! request per connection.
//!
//! Each subscriber is two threads (doorbells out, input in) and each
//! feeder one: blocking reads and the frames.rs condvar, no runtime.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::gpu::frames::ring::{self, Layout};
use crate::gpu::frames::{Ev, Info, Seen, SurfaceKey, Surfaces};
use crate::gpu::protocol::Rect;
use crate::paths::Paths;

/// Reads `rect` of a GPU resource into an image of the given stride (the
/// slice is the whole image; `rect` is where in it, as in the resource).
/// For records that name a virtio-gpu resource instead of carrying pixels:
/// set by the GPU device once it can (gpu::device::read_resource).
pub type ResourceReader = fn(u32, Rect, &mut [u8], usize) -> anyhow::Result<()>;
pub static RESOURCE_READER: OnceLock<ResourceReader> = OnceLock::new();

pub const DEFAULT_FPS: u32 = 60;
/// A doorbell at least this often, so a viewer can tell a quiet screen
/// from a dead daemon.
const KEEPALIVE: Duration = Duration::from_secs(1);
/// How long a feeder outlives its last viewer (a window being reopened,
/// the peek's next look).
const LINGER: Duration = Duration::from_secs(10);
/// A record line is a few hundred bytes; anything this long isn't one.
const MAX_LINE: usize = 64 * 1024;

/// What a viewer asks for.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DisplayReq {
    pub machine: String,
    /// The session whose screen to show, and where input goes.
    pub session: Option<String>,
    /// Show this scanout of the VM's GPU instead (input still goes to
    /// `session`, if given: the desktop session that drives the scanout).
    pub scanout: Option<u32>,
    /// Default 60, clamped to 1..=120.
    pub max_fps: Option<u32>,
    /// One PNG of the latest frame instead of a stream.
    pub once: bool,
}

impl DisplayReq {
    pub fn key(&self) -> Result<SurfaceKey, String> {
        if !valid_name(&self.machine) {
            return Err(format!("bad machine name {:?}", self.machine));
        }
        if let Some(s) = &self.session
            && !valid_name(s)
        {
            return Err(format!("bad session name {s:?}"));
        }
        match (&self.session, self.scanout) {
            (_, Some(n)) if n as usize >= crate::gpu::protocol::MAX_SCANOUTS => Err(format!("no scanout {n}")),
            (_, Some(n)) => Ok(SurfaceKey::Scanout(n)),
            (Some(s), None) => Ok(SurfaceKey::Session { machine: self.machine.clone(), session: s.clone() }),
            (None, None) => Err("which surface? Pass a session or a scanout".into()),
        }
    }

    pub fn fps(&self) -> u32 {
        self.max_fps.unwrap_or(DEFAULT_FPS).clamp(1, 120)
    }
}

/// Machine and session names go into header lines and file names.
fn valid_name(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && !s.starts_with('.') && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// How the daemon reaches the guest: the real one opens `gui <machine>`
/// through the agent (daemon.rs); tests fake it.
pub trait GuestConnector: Send + Sync {
    /// A connection to the agent's GUI side for `machine`, ready for a
    /// request line (docs/gui-protocol.md).
    fn open(&self, machine: &str) -> io::Result<UnixStream>;
}

/// Writes one activity-log line for a control toggle.
pub type ControlLog = Box<dyn Fn(&str, Option<&str>, bool) + Send + Sync>;

/// The feeders and input streams of one registry.
pub struct Hub {
    surfaces: &'static Surfaces,
    connector: Box<dyn GuestConnector>,
    log: ControlLog,
    linger: Duration,
    feeders: Mutex<HashMap<SurfaceKey, Feeder>>,
    inputs: Mutex<HashMap<(String, String), InputPipe>>,
    ids: AtomicU64,
}

struct Feeder {
    id: u64,
    viewers: usize,
    /// The agent connection, to shut down when the feeder stops.
    conn: Arc<Mutex<Option<UnixStream>>>,
    stop: Arc<AtomicBool>,
}

/// One `input.stream` per session, open while any viewer has control.
#[derive(Default)]
struct InputPipe {
    stream: Option<UnixStream>,
    holders: usize,
}

static HUB: OnceLock<Arc<Hub>> = OnceLock::new();

/// The daemon's hub, made on first use.
pub fn hub(make: impl FnOnce() -> Hub) -> Arc<Hub> {
    HUB.get_or_init(|| Arc::new(make())).clone()
}

impl Hub {
    pub fn new(surfaces: &'static Surfaces, connector: Box<dyn GuestConnector>, log: ControlLog) -> Hub {
        Hub { surfaces, connector, log, linger: LINGER, feeders: Mutex::default(), inputs: Mutex::default(), ids: AtomicU64::new(1) }
    }

    #[cfg(test)]
    fn lingering(mut self, linger: Duration) -> Hub {
        self.linger = linger;
        self
    }

    /// A viewer of `key`: a session's first starts its feeder. The feeder's
    /// id goes back to `unsubscribe`, so a viewer of a feeder that already
    /// ended doesn't count against the one that replaced it.
    fn subscribe(self: &Arc<Self>, key: &SurfaceKey) -> Option<u64> {
        let SurfaceKey::Session { .. } = key else { return None };
        let mut feeders = self.feeders.lock().unwrap();
        if let Some(f) = feeders.get_mut(key) {
            f.viewers += 1;
            return Some(f.id);
        }
        self.surfaces.begin(key);
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let conn = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        feeders.insert(key.clone(), Feeder { id, viewers: 1, conn: conn.clone(), stop: stop.clone() });
        let (hub, key) = (self.clone(), key.clone());
        std::thread::spawn(move || hub.run_feeder(key, id, conn, stop));
        Some(id)
    }

    /// A viewer of feeder `id` left: after the linger with none back, the
    /// feeder stops.
    fn unsubscribe(self: &Arc<Self>, key: &SurfaceKey, id: Option<u64>) {
        let mut feeders = self.feeders.lock().unwrap();
        let Some(f) = feeders.get_mut(key) else { return };
        if Some(f.id) != id {
            return;
        }
        f.viewers = f.viewers.saturating_sub(1);
        if f.viewers > 0 {
            return;
        }
        let (hub, key, id) = (self.clone(), key.clone(), f.id);
        std::thread::spawn(move || {
            std::thread::sleep(hub.linger);
            let mut feeders = hub.feeders.lock().unwrap();
            if let Some(f) = feeders.get(&key)
                && f.id == id
                && f.viewers == 0
            {
                f.stop.store(true, Ordering::SeqCst);
                if let Some(conn) = f.conn.lock().unwrap().as_ref() {
                    let _ = conn.shutdown(std::net::Shutdown::Both);
                }
                // The feeder forgets the surface as it exits: it may be
                // writing one last frame right now.
                feeders.remove(&key);
            }
        });
    }

    fn run_feeder(self: Arc<Self>, key: SurfaceKey, id: u64, conn: Arc<Mutex<Option<UnixStream>>>, stop: Arc<AtomicBool>) {
        let SurfaceKey::Session { machine, session } = &key else { return };
        let result = (|| {
            let stream = self.connector.open(machine).map_err(|e| format!("can't reach the VM's agent: {e}"))?;
            *conn.lock().unwrap() = Some(stream.try_clone().map_err(|e| e.to_string())?);
            if stop.load(Ordering::SeqCst) {
                return Ok(());
            }
            let args = json!({"format": "bgra8", "max_fps": DEFAULT_FPS});
            let mut r = BufReader::new(&stream);
            open_stream(&mut r, &stream, session, "screen.stream", args)?;
            feed(&mut r, self.surfaces, &key)
        })();
        // Under the feeders lock, so a feeder started meanwhile for the
        // same session keeps its surface.
        let mut feeders = self.feeders.lock().unwrap();
        if feeders.get(&key).is_some_and(|f| f.id == id) {
            feeders.remove(&key);
        }
        if feeders.contains_key(&key) {
            return;
        }
        if stop.load(Ordering::SeqCst) {
            // Stopped with nobody watching: the surface and its ring go.
            self.surfaces.forget(&key);
        } else {
            let error = result.err().unwrap_or_else(|| "the session's screen stream ended".into());
            eprintln!("display: {key}: {error}");
            self.surfaces.end(&key, &error);
        }
    }

    /// A viewer took control of `session`'s input.
    fn take_control(&self, machine: &str, session: &str) {
        self.inputs.lock().unwrap().entry((machine.into(), session.into())).or_default().holders += 1;
        (self.log)(machine, Some(session), true);
    }

    /// A viewer let go: everything it held is released, and the pipe closes
    /// once nobody has control.
    fn release_control(&self, machine: &str, session: &str) {
        let mut inputs = self.inputs.lock().unwrap();
        let k = (machine.to_string(), session.to_string());
        if let Some(pipe) = inputs.get_mut(&k) {
            // Nothing to release if nothing was ever sent.
            if let Some(mut stream) = pipe.stream.as_ref()
                && stream.write_all(b"{\"ev\":\"release_all\"}\n").is_err()
            {
                pipe.stream = None;
            }
            pipe.holders = pipe.holders.saturating_sub(1);
            if pipe.holders == 0 {
                inputs.remove(&k);
            }
        }
        drop(inputs);
        (self.log)(machine, Some(session), false);
    }

    /// Write one input line to the session's `input.stream`, opening it
    /// (again, once, if the old one broke).
    fn send_input(&self, machine: &str, session: &str, line: &str) {
        let mut inputs = self.inputs.lock().unwrap();
        let pipe = inputs.entry((machine.into(), session.into())).or_default();
        for _ in 0..2 {
            if pipe.stream.is_none() {
                match self.open_input(machine, session) {
                    Ok(s) => pipe.stream = Some(s),
                    Err(err) => {
                        eprintln!("display: input for {machine} session {session}: {err}");
                        return;
                    }
                }
            }
            let mut stream = pipe.stream.as_ref().unwrap();
            if stream.write_all(format!("{line}\n").as_bytes()).is_ok() {
                return;
            }
            pipe.stream = None;
        }
    }

    fn open_input(&self, machine: &str, session: &str) -> Result<UnixStream, String> {
        let stream = self.connector.open(machine).map_err(|e| format!("can't reach the VM's agent: {e}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(10))).map_err(|e| e.to_string())?;
        // Writes happen under the inputs lock: an agent that stops reading
        // must break the pipe, not wedge every viewer's input.
        stream.set_write_timeout(Some(Duration::from_secs(2))).map_err(|e| e.to_string())?;
        open_stream(&mut BufReader::new(&stream), &stream, session, "input.stream", json!({}))?;
        Ok(stream)
    }
}

/// Send a stream verb's request line and check its reply line.
fn open_stream(r: &mut impl BufRead, mut w: &UnixStream, session: &str, verb: &str, args: Value) -> Result<(), String> {
    let request = json!({"v": crate::gui::proto::VERSION, "session": session, "verb": verb, "args": args});
    w.write_all(format!("{request}\n").as_bytes()).map_err(|e| format!("sending {verb}: {e}"))?;
    let line = read_line(r).map_err(|e| format!("reading the {verb} reply: {e}"))?.ok_or_else(|| format!("the agent closed {verb}"))?;
    let reply: Value = serde_json::from_str(&line).map_err(|_| format!("unexpected {verb} reply {line:?}"))?;
    if reply["ok"] != true {
        return Err(reply["error"].as_str().map_or_else(|| format!("the agent refused {verb}"), str::to_string));
    }
    Ok(())
}

/// One line without its newline, None at EOF, an error past MAX_LINE.
fn read_line(r: &mut impl BufRead) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let n = r.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if line.last() != Some(&b'\n') && line.len() > MAX_LINE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
    }
    while line.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
        line.pop();
    }
    String::from_utf8(line).map(Some).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))
}

/// A record of `screen.stream` (docs/display-protocol.md).
#[derive(Deserialize, Debug, Default)]
struct Record {
    #[serde(default)]
    w: u32,
    #[serde(default)]
    h: u32,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    damage: Vec<[u32; 4]>,
    #[serde(default)]
    payload_len: u64,
    #[serde(default)]
    res_id: Option<u32>,
    #[serde(default)]
    error: Option<String>,
}

/// Read records into `key`'s surface until the stream ends (Ok) or breaks
/// the protocol or reports an error (Err).
pub fn feed(r: &mut impl BufRead, surfaces: &Surfaces, key: &SurfaceKey) -> Result<(), String> {
    while let Some(line) = read_line(r).map_err(|e| format!("reading the screen stream: {e}"))? {
        if line.is_empty() {
            continue;
        }
        let rec: Record = serde_json::from_str(&line).map_err(|e| format!("bad screen record ({e}): {}", truncated(&line)))?;
        if let Some(error) = rec.error {
            return Err(error);
        }
        match rec.format.as_deref() {
            Some("bgra8") => pixels(r, surfaces, key, &rec)?,
            Some("resource") => resource(surfaces, key, &rec)?,
            other => return Err(format!("unknown screen record format {other:?}")),
        }
    }
    Ok(())
}

fn truncated(s: &str) -> String {
    s.chars().take(200).collect()
}

/// The record's damage, checked against its size.
fn rects(rec: &Record, w: u32, h: u32) -> Result<Vec<Rect>, String> {
    if Layout::new(w, h).is_none() {
        return Err(format!("bad screen size {w}x{h}"));
    }
    let rects: Vec<Rect> = rec.damage.iter().map(|d| Rect::new(d[0], d[1], d[2], d[3])).filter(|r| !r.is_empty()).collect();
    if let Some(r) = rects.iter().find(|r| !r.fits(w, h)) {
        return Err(format!("damage {r:?} is outside the {w}x{h} screen"));
    }
    Ok(rects)
}

fn pixels(r: &mut impl BufRead, surfaces: &Surfaces, key: &SurfaceKey, rec: &Record) -> Result<(), String> {
    let rects = rects(rec, rec.w, rec.h)?;
    let want: u64 = rects.iter().map(|r| r.width as u64 * r.height as u64 * 4).sum();
    if rec.payload_len != want {
        return Err(format!("payload_len {} for damage of {want} bytes", rec.payload_len));
    }
    let mut payload = vec![0u8; want as usize];
    r.read_exact(&mut payload).map_err(|e| format!("reading a frame: {e}"))?;
    surfaces.update_parts(key, rec.w, rec.h, &rects, |px, stride| {
        let mut at = 0;
        for rect in &rects {
            let n = rect.width as usize * rect.height as usize * 4;
            ring::blit(px, stride, *rect, &payload[at..at + n]);
            at += n;
        }
    });
    Ok(())
}

/// A record naming a GPU resource: its pixels come from the device.
fn resource(surfaces: &Surfaces, key: &SurfaceKey, rec: &Record) -> Result<(), String> {
    let reader = RESOURCE_READER.get().ok_or("the screen stream sent a GPU resource, which this berth can't read yet")?;
    let res_id = rec.res_id.ok_or("a resource record without res_id")?;
    if rec.payload_len != 0 {
        return Err("a resource record with a payload".into());
    }
    let (w, h) = match (rec.w, rec.h, surfaces.info(key)) {
        (0, 0, Some(i)) => (i.width, i.height),
        (w, h, _) => (w, h),
    };
    let rects = rects(rec, w, h)?;
    let mut failed = None;
    surfaces.update_parts(key, w, h, &rects, |px, stride| {
        for rect in &rects {
            if let Err(err) = reader(res_id, *rect, px, stride) {
                failed = Some(format!("reading GPU resource {res_id}: {err:#}"));
                return;
            }
        }
    });
    failed.map_or(Ok(()), Err)
}

/// An input line the viewer sent.
#[derive(Debug, PartialEq)]
pub enum Input {
    /// Take (true) or give back control.
    Control(bool),
    /// An event for the guest, forwarded as it came.
    Event,
}

/// Check an input line against docs/display-protocol.md, with the
/// surface's size for pointer positions.
pub fn check_input(line: &str, size: Option<(u32, u32)>) -> Result<Input, String> {
    let v: Value = serde_json::from_str(line).map_err(|_| "not JSON".to_string())?;
    let num = |k: &str| v[k].as_f64().filter(|n| n.is_finite()).ok_or_else(|| format!("{k} isn't a number"));
    let flag = |k: &str| v[k].as_bool().ok_or_else(|| format!("{k} isn't true or false"));
    match v["ev"].as_str().ok_or("no ev")? {
        "pointer" => {
            let (w, h) = size.ok_or("no screen yet")?;
            let (x, y) = (num("x")?, num("y")?);
            if !(0.0..w as f64).contains(&x) || !(0.0..h as f64).contains(&y) {
                return Err(format!("{x},{y} is off the {w}x{h} screen"));
            }
            Ok(Input::Event)
        }
        "button" => {
            let button = v["button"].as_u64().ok_or("button isn't a number")?;
            if !matches!(button, 272..=274) {
                return Err(format!("button {button} isn't 272, 273 or 274"));
            }
            flag("down")?;
            Ok(Input::Event)
        }
        "scroll" => {
            let (dx, dy) = (num("dx")?, num("dy")?);
            if dx.abs() > 10_000.0 || dy.abs() > 10_000.0 {
                return Err("scroll too far".into());
            }
            if !(v["discrete"].is_null() || v["discrete"].is_boolean() || v["discrete"].is_number()) {
                return Err("discrete isn't true, false or a number".into());
            }
            Ok(Input::Event)
        }
        "key" => {
            let code = v["code"].as_u64().ok_or("code isn't a number")?;
            if code >= 0x300 {
                return Err(format!("key code {code} isn't an evdev key"));
            }
            flag("down")?;
            Ok(Input::Event)
        }
        "release_all" => Ok(Input::Event),
        "control" => Ok(Input::Control(flag("on")?)),
        other => Err(format!("unknown ev {other:?}")),
    }
}

/// The ring part of the reply and of a resize doorbell.
fn ring_fields(info: Option<&Info>) -> serde_json::Map<String, Value> {
    let v = match info {
        Some(i) => json!({
            "ring": i.path, "gen": i.generation, "width": i.width, "height": i.height, "stride": i.stride,
            "format": "bgra8", "slots": ring::SLOTS, "live": true,
        }),
        None => json!({"ring": null, "gen": 0, "width": 0, "height": 0, "stride": 0, "format": "bgra8", "slots": ring::SLOTS, "live": false}),
    };
    let Value::Object(map) = v else { unreachable!() };
    map
}

fn send(mut w: &UnixStream, v: &Value) -> io::Result<()> {
    w.write_all(format!("{v}\n").as_bytes())
}

/// Serve one `display` request on `stream` (the daemon has read its line;
/// `leftover` is whatever came after it). Returns when the viewer leaves.
pub fn serve(hub: &Arc<Hub>, stream: UnixStream, leftover: Vec<u8>, req: DisplayReq) {
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let key = match req.key() {
        Ok(k) => k,
        Err(error) => {
            let _ = send(&stream, &json!({"ok": false, "error": error}));
            return;
        }
    };
    if req.once {
        match hub.surfaces.png(&key) {
            Some(png) => {
                let _ = send(&stream, &json!({"ok": true, "payload_len": png.len()})).and_then(|()| (&stream).write_all(&png));
            }
            None => {
                let _ = send(&stream, &json!({"ok": false, "error": "no live surface"}));
            }
        }
        return;
    }
    let feeder = hub.subscribe(&key);
    let info = hub.surfaces.info(&key);
    let mut reply = ring_fields(info.as_ref());
    reply.insert("ok".into(), json!(true));
    reply.insert("transports".into(), json!(["ring"]));
    if send(&stream, &Value::Object(reply)).is_ok() {
        let done = Arc::new(AtomicBool::new(false));
        let input = match stream.try_clone() {
            Ok(s) => {
                let (hub, req, key, done) = (hub.clone(), req.clone(), key.clone(), done.clone());
                Some(std::thread::spawn(move || read_input(&hub, &req, &key, s, leftover, &done)))
            }
            Err(_) => None,
        };
        let mut seen = Seen { generation: info.as_ref().map_or(0, |i| i.generation), frame: 0, cursor: 0 };
        let _ = doorbells(hub.surfaces, &key, &stream, &mut seen, req.fps(), &done);
        let _ = stream.shutdown(std::net::Shutdown::Both);
        if let Some(t) = input {
            let _ = t.join();
        }
    }
    hub.unsubscribe(&key, feeder);
}

/// Tell the viewer about every change, at most `fps` a second: frames and
/// cursor moves within one interval collapse into the newest (its damage
/// the union), a resize and an end go at once.
fn doorbells(surfaces: &Surfaces, key: &SurfaceKey, stream: &UnixStream, seen: &mut Seen, fps: u32, done: &AtomicBool) -> io::Result<()> {
    let interval = Duration::from_secs(1) / fps;
    let mut last_sent: Option<Instant> = None;
    let mut last_any = Instant::now();
    while !done.load(Ordering::SeqCst) {
        let ev = surfaces.wait_newer(key, seen, KEEPALIVE.saturating_sub(last_any.elapsed()));
        if matches!(ev, Ev::Frame { .. } | Ev::Cursor { .. })
            && let Some(wait) = last_sent.map(|t| (t + interval).saturating_duration_since(Instant::now()))
            && !wait.is_zero()
        {
            std::thread::sleep(wait);
            continue;
        }
        match ev {
            Ev::Timeout => send(stream, &json!({"ev": "keepalive"}))?,
            Ev::End(error) => return send(stream, &json!({"ev": "end", "error": error})),
            Ev::Resize(info) => {
                let mut v = ring_fields(info.as_ref());
                v.insert("ev".into(), json!("resize"));
                send(stream, &Value::Object(v))?;
                seen.generation = info.map_or(0, |i| i.generation);
            }
            Ev::Frame { slot, seq, damage, .. } => {
                send(stream, &json!({"ev": "frame", "slot": slot, "seq": seq, "damage": [[damage.x, damage.y, damage.width, damage.height]]}))?;
                seen.frame = seq;
                last_sent = Some(Instant::now());
            }
            Ev::Cursor { seq } => {
                send(stream, &json!({"ev": "cursor", "seq": seq}))?;
                seen.cursor = seq;
                last_sent = Some(Instant::now());
            }
        }
        last_any = Instant::now();
    }
    Ok(())
}

/// The viewer's input lines, until it hangs up. Control needs a session.
fn read_input(hub: &Hub, req: &DisplayReq, key: &SurfaceKey, stream: UnixStream, leftover: Vec<u8>, done: &AtomicBool) {
    let _ = stream.set_read_timeout(None);
    let mut r = BufReader::new(io::Cursor::new(leftover).chain(&stream));
    let mut control = false;
    let mut dropped = 0u32;
    while let Ok(Some(line)) = read_line(&mut r) {
        if line.trim().is_empty() {
            continue;
        }
        let size = hub.surfaces.info(key).map(|i| (i.width, i.height));
        match check_input(&line, size) {
            Ok(Input::Control(on)) => {
                if let Some(session) = &req.session
                    && on != control
                {
                    control = on;
                    if on {
                        hub.take_control(&req.machine, session);
                    } else {
                        hub.release_control(&req.machine, session);
                    }
                }
            }
            Ok(Input::Event) => {
                if control && let Some(session) = &req.session {
                    hub.send_input(&req.machine, session, line.trim());
                }
            }
            Err(err) => {
                // A misbehaving viewer shouldn't fill the daemon's log.
                dropped += 1;
                if dropped <= 5 {
                    eprintln!("display: {key}: dropped an input line ({err})");
                }
            }
        }
    }
    if control && let Some(session) = &req.session {
        hub.release_control(&req.machine, session);
    }
    done.store(true, Ordering::SeqCst);
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

// The client side, for the berth CLI.

/// Open a display stream: the connection and its reply.
pub fn connect(paths: &Paths, req: &DisplayReq) -> Result<(BufReader<UnixStream>, Value)> {
    let mut stream = UnixStream::connect(paths.control_socket()).context("berth is not running (berth start)")?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let request = crate::daemon::Request::Display {
        machine: req.machine.clone(),
        session: req.session.clone(),
        scanout: req.scanout,
        max_fps: req.max_fps,
        once: req.once,
    };
    writeln!(stream, "{}", serde_json::to_string(&request)?)?;
    let mut r = BufReader::new(stream);
    // A daemon from before the display stream doesn't know the request
    // and hangs up.
    let line = read_line(&mut r)?.context("the daemon has no display stream (restart berth)")?;
    let reply: Value = serde_json::from_str(&line).with_context(|| format!("unexpected daemon reply {line:?}"))?;
    if reply["ok"] != true {
        bail!("{}", reply["error"].as_str().unwrap_or("the daemon refused the display stream"));
    }
    Ok((r, reply))
}

/// The latest frame of a session's (or scanout's) surface as a PNG, from
/// the daemon's registry: fast, but only while something feeds it.
pub fn screenshot(paths: &Paths, machine: &str, session: Option<&str>, scanout: Option<u32>) -> Result<Vec<u8>> {
    let req = DisplayReq { machine: machine.into(), session: session.map(str::to_string), scanout, once: true, ..DisplayReq::default() };
    let (mut r, reply) = connect(paths, &req)?;
    let len = reply["payload_len"].as_u64().context("no payload_len")?;
    if len > 1 << 30 {
        bail!("a {len}-byte screenshot");
    }
    let mut png = vec![0u8; len as usize];
    r.read_exact(&mut png)?;
    Ok(png)
}

/// `berth gui watch`: follow a surface and print its frame rate and damage
/// once a second; `frames` stops after that many.
pub fn watch(paths: &Paths, req: &DisplayReq, frames: Option<u64>) -> Result<()> {
    let (mut r, reply) = connect(paths, req)?;
    let mut ring = map_ring(&reply)?;
    let describe = |ring: &Option<ring::Ring>| match ring {
        Some(r) => format!("{}x{} (gen {})", r.width(), r.height(), r.generation()),
        None => "no live surface yet".into(),
    };
    println!("watching {}: {}", req.key().map_err(anyhow::Error::msg)?, describe(&ring));
    let (mut count, mut total, mut torn, mut damaged) = (0u64, 0u64, 0u64, 0u64);
    let mut since = Instant::now();
    let mut copy = Vec::new();
    r.get_ref().set_read_timeout(Some(Duration::from_secs(5)))?;
    while let Some(line) = read_line(&mut r)? {
        let ev: Value = serde_json::from_str(&line).with_context(|| format!("unexpected doorbell {line:?}"))?;
        match ev["ev"].as_str() {
            Some("frame") => {
                count += 1;
                total += 1;
                if let Some(d) = ev["damage"][0].as_array() {
                    damaged += d.get(2).and_then(Value::as_u64).unwrap_or(0) * d.get(3).and_then(Value::as_u64).unwrap_or(0);
                }
                if let (Some(ring), Some(slot)) = (&ring, ev["slot"].as_u64())
                    && ring.read_slot(slot as usize % ring::SLOTS, &mut copy) != ev["seq"].as_u64()
                {
                    torn += 1;
                }
            }
            Some("resize") => {
                ring = map_ring(&ev)?;
                println!("resized: {}", describe(&ring));
            }
            Some("end") => bail!("the stream ended: {}", ev["error"].as_str().unwrap_or("no reason given")),
            _ => {}
        }
        if since.elapsed() >= Duration::from_secs(1) {
            let secs = since.elapsed().as_secs_f64();
            let area = ring.as_ref().map_or(0, |r| r.width() as u64 * r.height() as u64).max(1);
            println!(
                "{:5.1} fps  damage {:5.1}% of the screen a frame{}",
                count as f64 / secs,
                if count > 0 { damaged as f64 * 100.0 / (count * area) as f64 } else { 0.0 },
                if torn > 0 { format!("  ({torn} overwritten before read)") } else { String::new() }
            );
            (count, damaged, torn) = (0, 0, 0);
            since = Instant::now();
        }
        if frames.is_some_and(|n| total >= n) {
            break;
        }
    }
    Ok(())
}

fn map_ring(v: &Value) -> Result<Option<ring::Ring>> {
    if v["live"] != true {
        return Ok(None);
    }
    let path = v["ring"].as_str().context("a live surface without a ring file")?;
    Ok(Some(ring::Ring::open_read(std::path::Path::new(path))?))
}

#[cfg(test)]
#[path = "display_tests.rs"]
mod tests;
