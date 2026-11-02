use std::sync::mpsc;

use super::*;

fn leak() -> &'static Surfaces {
    Box::leak(Box::new(Surfaces::new()))
}

fn rings_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("berth-display-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Each connection the daemon opens to the "agent" lands here.
struct Fake(Mutex<mpsc::Sender<UnixStream>>);

impl GuestConnector for Fake {
    fn open(&self, _machine: &str) -> io::Result<UnixStream> {
        let (daemon, agent) = UnixStream::pair()?;
        self.0.lock().unwrap().send(agent).unwrap();
        Ok(daemon)
    }
}

type Log = Arc<Mutex<Vec<(String, Option<String>, bool)>>>;

fn hub(surfaces: &'static Surfaces) -> (Arc<Hub>, mpsc::Receiver<UnixStream>, Log) {
    let (tx, rx) = mpsc::channel();
    let log: Log = Arc::default();
    let l = log.clone();
    let write = Box::new(move |m: &str, s: Option<&str>, on: bool| l.lock().unwrap().push((m.into(), s.map(str::to_string), on)));
    let hub = Hub::new(surfaces, Box::new(Fake(Mutex::new(tx))), write).lingering(Duration::from_millis(50));
    (Arc::new(hub), rx, log)
}

/// The agent's side of the next connection: the request it got.
fn agent(rx: &mpsc::Receiver<UnixStream>) -> (BufReader<UnixStream>, Value) {
    let stream = rx.recv_timeout(Duration::from_secs(5)).expect("the daemon connects to the agent");
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut r = BufReader::new(stream);
    let line = read_line(&mut r).unwrap().unwrap();
    (r, serde_json::from_str(&line).unwrap())
}

/// A viewer connected to `serve`.
fn viewer(hub: &Arc<Hub>, req: DisplayReq) -> (BufReader<UnixStream>, Value) {
    let (mine, daemon) = UnixStream::pair().unwrap();
    let hub = hub.clone();
    std::thread::spawn(move || serve(&hub, daemon, Vec::new(), req));
    mine.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut r = BufReader::new(mine);
    let reply = next(&mut r);
    (r, reply)
}

fn next(r: &mut BufReader<UnixStream>) -> Value {
    serde_json::from_str(&read_line(r).unwrap().expect("a line")).unwrap()
}

/// The next doorbell that isn't a keepalive.
fn bell(r: &mut BufReader<UnixStream>) -> Value {
    loop {
        let v = next(r);
        if v["ev"] != "keepalive" {
            return v;
        }
    }
}

fn record(w: u32, h: u32, damage: &[[u32; 4]], fill: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
    let mut payload = Vec::new();
    for d in damage {
        for y in d[1]..d[1] + d[3] {
            for x in d[0]..d[0] + d[2] {
                payload.extend_from_slice(&fill(x, y));
            }
        }
    }
    let mut out = json!({"seq": 1, "w": w, "h": h, "stride": w * 4, "format": "bgra8", "damage": damage, "payload_len": payload.len()}).to_string().into_bytes();
    out.push(b'\n');
    out.extend_from_slice(&payload);
    out
}

fn session_req(session: &str) -> DisplayReq {
    DisplayReq { machine: "dev".into(), session: Some(session.into()), ..DisplayReq::default() }
}

#[test]
fn requests_name_one_surface() {
    assert_eq!(session_req("s-1").key(), Ok(SurfaceKey::Session { machine: "dev".into(), session: "s-1".into() }));
    let scanout = DisplayReq { scanout: Some(0), ..session_req("s-1") };
    assert_eq!(scanout.key(), Ok(SurfaceKey::Scanout(0)));
    assert!(DisplayReq { scanout: Some(16), ..scanout.clone() }.key().is_err());
    assert!(DisplayReq { machine: "dev".into(), ..DisplayReq::default() }.key().is_err());
    assert!(DisplayReq { machine: "../x".into(), ..scanout.clone() }.key().is_err());
    assert!(session_req("a b").key().is_err());
    assert_eq!(DisplayReq::default().fps(), 60);
}

#[test]
fn the_feeder_parses_records_into_the_ring() {
    let surfaces = leak();
    let key = SurfaceKey::Session { machine: "dev".into(), session: "f".into() };
    let mut fixture = record(4, 2, &[[0, 0, 4, 2]], |x, y| [x as u8, y as u8, 0, 255]);
    fixture.extend(record(4, 2, &[[0, 0, 1, 1], [3, 1, 1, 1]], |_, _| [9, 9, 9, 9]));
    fixture.extend(b"{\"format\":\"resource\",\"res_id\":7,\"damage\":[[0,0,4,2]],\"payload_len\":0}\n");
    let mut r = BufReader::new(&fixture[..]);
    let err = feed_with(&mut r, surfaces, &key, None).unwrap_err();
    assert!(err.contains("GPU resource"), "{err}");
    let frame = surfaces.frame(&key).unwrap();
    let at = |x: usize, y: usize| frame.pixels[(y * 4 + x) * 4..][..4].to_vec();
    assert_eq!((frame.width, frame.height), (4, 2));
    assert_eq!(at(0, 0), [9, 9, 9, 9]);
    assert_eq!(at(3, 1), [9, 9, 9, 9]);
    assert_eq!(at(1, 0), [1, 0, 0, 255], "outside the damage");
    assert_eq!(at(2, 1), [2, 1, 0, 255]);

    // Broken streams are errors, a clean end isn't.
    let bad = |text: &[u8]| feed(&mut BufReader::new(text), surfaces, &key).unwrap_err();
    assert!(bad(b"{\"format\":\"bgra8\",\"w\":4,\"h\":2,\"damage\":[[0,0,4,2]],\"payload_len\":3}\n").contains("payload_len"));
    assert!(bad(b"{\"format\":\"bgra8\",\"w\":4,\"h\":2,\"damage\":[[3,0,4,2]],\"payload_len\":32}\n").contains("outside"));
    assert!(bad(b"{\"format\":\"bgra8\",\"w\":0,\"h\":2,\"damage\":[],\"payload_len\":0}\n").contains("size"));
    assert!(bad(b"{\"format\":\"yuv\"}\n").contains("format"));
    assert_eq!(bad(b"{\"error\":\"session ended\"}\n"), "session ended");
    assert!(bad(b"not json\n").contains("bad screen record"));
    assert!(feed(&mut BufReader::new(&b""[..]), surfaces, &key).is_ok());
}

#[test]
fn input_lines_are_checked() {
    let size = Some((1920, 1080));
    let ok = |l: &str| check_input(l, size);
    assert_eq!(ok(r#"{"ev":"pointer","x":10,"y":1079}"#), Ok(Input::Event));
    assert!(ok(r#"{"ev":"pointer","x":1920,"y":0}"#).is_err());
    assert!(ok(r#"{"ev":"pointer","x":-1,"y":0}"#).is_err());
    assert!(check_input(r#"{"ev":"pointer","x":1,"y":1}"#, None).is_err());
    assert_eq!(ok(r#"{"ev":"button","button":272,"down":true}"#), Ok(Input::Event));
    assert!(ok(r#"{"ev":"button","button":275,"down":true}"#).is_err());
    assert!(ok(r#"{"ev":"button","button":272}"#).is_err());
    assert_eq!(ok(r#"{"ev":"scroll","dx":0,"dy":-15,"discrete":true}"#), Ok(Input::Event));
    assert_eq!(ok(r#"{"ev":"scroll","dx":0.5,"dy":1}"#), Ok(Input::Event));
    assert!(ok(r#"{"ev":"scroll","dx":"a","dy":1}"#).is_err());
    assert_eq!(ok(r#"{"ev":"key","code":30,"down":false}"#), Ok(Input::Event));
    assert!(ok(r#"{"ev":"key","code":768,"down":true}"#).is_err());
    assert_eq!(ok(r#"{"ev":"release_all"}"#), Ok(Input::Event));
    assert_eq!(ok(r#"{"ev":"control","on":true}"#), Ok(Input::Control(true)));
    assert!(ok(r#"{"ev":"control"}"#).is_err());
    assert!(ok(r#"{"ev":"teleport"}"#).is_err());
    assert!(ok("nope").is_err());
}

#[test]
fn once_without_a_feeder_answers_at_once() {
    let (hub, rx, _) = hub(leak());
    let started = Instant::now();
    let (_, reply) = viewer(&hub, DisplayReq { once: true, ..session_req("none") });
    assert!(started.elapsed() < Duration::from_millis(10), "{:?}", started.elapsed());
    assert_eq!(reply, json!({"ok": false, "error": "no live surface"}));
    assert!(rx.try_recv().is_err(), "once starts no feeder");
}

#[test]
fn once_sends_a_png_of_a_live_surface() {
    let surfaces = leak();
    surfaces.update(&SurfaceKey::Scanout(0), 3, 2, Rect::new(0, 0, 3, 2), |px, _| px.fill(0x80));
    let (hub, _rx, _) = hub(surfaces);
    let (mut r, reply) = viewer(&hub, DisplayReq { machine: "dev".into(), scanout: Some(0), once: true, ..DisplayReq::default() });
    let mut png = vec![0u8; reply["payload_len"].as_u64().unwrap() as usize];
    r.read_exact(&mut png).unwrap();
    assert!(png.starts_with(b"\x89PNG"));
    assert_eq!(crate::gui::png_size(&png), (3, 2));
}

#[test]
fn a_session_stream_from_feeder_to_ring() {
    let surfaces = leak();
    surfaces.set_dir(&rings_dir("stream")).unwrap();
    let (hub, rx, _) = hub(surfaces);
    let (mut view, reply) = viewer(&hub, DisplayReq { max_fps: Some(120), ..session_req("s-1") });
    assert_eq!((reply["ok"].clone(), reply["transports"].clone(), reply["live"].clone()), (json!(true), json!(["ring"]), json!(false)));

    // The feeder asked the agent for the session's screen.
    let (agent_r, request) = agent(&rx);
    assert_eq!(request, json!({"v": 1, "session": "s-1", "verb": "screen.stream", "args": {"format": "bgra8", "max_fps": 60}}));
    let mut agent_w = agent_r.get_ref().try_clone().unwrap();
    agent_w.write_all(b"{\"ok\":true,\"payload_len\":0}\n").unwrap();
    agent_w.write_all(&record(64, 32, &[[0, 0, 64, 32]], |x, _| [x as u8, 1, 2, 255])).unwrap();

    let resize = bell(&mut view);
    assert_eq!((resize["ev"].clone(), resize["width"].clone(), resize["height"].clone(), resize["stride"].clone()), (json!("resize"), json!(64), json!(32), json!(256)));
    assert_eq!((resize["format"].clone(), resize["slots"].clone(), resize["live"].clone()), (json!("bgra8"), json!(3), json!(true)));
    let ring = ring::Ring::open_read(std::path::Path::new(resize["ring"].as_str().unwrap())).unwrap();
    assert_eq!(ring.generation(), resize["gen"].as_u64().unwrap());
    let frame = bell(&mut view);
    assert_eq!(frame["ev"], "frame");
    assert_eq!(frame["damage"], json!([[0, 0, 64, 32]]));
    let mut px = Vec::new();
    assert_eq!(ring.read_slot(frame["slot"].as_u64().unwrap() as usize, &mut px), frame["seq"].as_u64());
    assert_eq!(&px[40 * 4..40 * 4 + 4], &[40, 1, 2, 255]);

    // A small change: only its damage, in the next slot.
    agent_w.write_all(&record(64, 32, &[[5, 6, 2, 2]], |_, _| [7, 7, 7, 7])).unwrap();
    let small = bell(&mut view);
    assert_eq!(small["damage"], json!([[5, 6, 2, 2]]));
    assert_ne!(small["slot"], frame["slot"]);
    let slot = small["slot"].as_u64().unwrap() as usize;
    assert!(ring.read_slot(slot, &mut px).is_some());
    assert_eq!(&px[6 * 256 + 5 * 4..][..4], &[7, 7, 7, 7]);
    assert_eq!(&px[40 * 4..][..4], &[40, 1, 2, 255], "carried over from the frame before");

    // A second viewer shares the feeder.
    let (mut view2, reply2) = viewer(&hub, DisplayReq { max_fps: Some(120), ..session_req("s-1") });
    assert_eq!(reply2["live"], true);
    assert_eq!(bell(&mut view2)["damage"], json!([[0, 0, 64, 32]]), "a new viewer starts with a full frame");
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err());

    // The agent ends the stream: both viewers hear why.
    agent_w.write_all(b"{\"error\":\"session ended\"}\n").unwrap();
    assert_eq!(bell(&mut view), json!({"ev": "end", "error": "session ended"}));
    assert_eq!(bell(&mut view2), json!({"ev": "end", "error": "session ended"}));
}

#[test]
fn the_feeder_stops_after_the_last_viewer() {
    let surfaces = leak();
    let (hub, rx, _) = hub(surfaces);
    let key = SurfaceKey::Session { machine: "dev".into(), session: "s-2".into() };
    let (view, _) = viewer(&hub, session_req("s-2"));
    let (mut agent_r, _) = agent(&rx);
    let mut agent_w = agent_r.get_ref().try_clone().unwrap();
    agent_w.write_all(b"{\"ok\":true}\n").unwrap();
    agent_w.write_all(&record(8, 8, &[[0, 0, 8, 8]], |_, _| [1; 4])).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while surfaces.info(&key).is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(surfaces.info(&key).is_some());
    drop(view);
    // The agent's side sees the stream close once the linger is over.
    let mut rest = Vec::new();
    let _ = agent_r.read_to_end(&mut rest);
    let deadline = Instant::now() + Duration::from_secs(5);
    while surfaces.info(&key).is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(surfaces.info(&key).is_none(), "the surface goes with its feeder");
    assert!(hub.feeders.lock().unwrap().is_empty());
}

/// A viewer of a feeder that ended leaving late doesn't count against
/// the feeder a newer viewer started meanwhile.
#[test]
fn a_late_leaver_of_an_ended_feeder_keeps_the_new_one() {
    let surfaces = leak();
    let (hub, rx, _) = hub(surfaces);
    let key = SurfaceKey::Session { machine: "dev".into(), session: "s-5".into() };
    let first = hub.subscribe(&key);
    let (agent_r, _) = agent(&rx);
    agent_r.get_ref().write_all(b"{\"ok\":false,\"error\":\"no such session\"}\n").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !hub.feeders.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(hub.feeders.lock().unwrap().is_empty(), "the failed feeder is gone");
    let second = hub.subscribe(&key);
    assert_ne!(first, second);
    let _agent2 = agent(&rx);
    hub.unsubscribe(&key, first);
    std::thread::sleep(Duration::from_millis(150));
    let feeders = hub.feeders.lock().unwrap();
    let f = feeders.get(&key).expect("the new feeder outlives the linger");
    assert_eq!((Some(f.id), f.viewers), (second, 1));
}

#[test]
fn doorbells_are_paced_to_max_fps() {
    let surfaces = leak();
    let key = SurfaceKey::Scanout(0);
    surfaces.update(&key, 128, 1, Rect::new(0, 0, 128, 1), |_, _| {});
    let (hub, _rx, _) = hub(surfaces);
    let (mut view, reply) = viewer(&hub, DisplayReq { machine: "dev".into(), scanout: Some(0), max_fps: Some(10), ..DisplayReq::default() });
    assert_eq!(reply["live"], true);
    assert_eq!(bell(&mut view)["damage"], json!([[0, 0, 128, 1]]));
    // 100 frames at 100 Hz, frame i changing pixel i.
    let producer = std::thread::spawn(move || {
        for i in 0..100u32 {
            surfaces.update(&key, 128, 1, Rect::new(i, 0, 1, 1), |px, _| px[i as usize * 4] = 1);
            std::thread::sleep(Duration::from_millis(10));
        }
        surfaces.frame(&key).unwrap().sequence
    });
    let started = Instant::now();
    let mut covered = [false; 100];
    let (mut events, mut last_seq, mut secs) = (0, 0, 0.0);
    loop {
        let v = next(&mut view);
        // Idle for a second once the producer is done: everything's in.
        if v["ev"] == "keepalive" {
            if producer.is_finished() {
                break;
            }
            continue;
        }
        assert_eq!(v["ev"], "frame");
        events += 1;
        secs = started.elapsed().as_secs_f64();
        let d = v["damage"][0].as_array().unwrap().iter().map(|n| n.as_u64().unwrap() as usize).collect::<Vec<_>>();
        covered[d[0]..(d[0] + d[2]).min(100)].iter_mut().for_each(|c| *c = true);
        last_seq = v["seq"].as_u64().unwrap();
    }
    let last = producer.join().unwrap();
    assert_eq!(last_seq, last, "the last doorbell is the last frame");
    assert!(covered.iter().all(|c| *c), "every frame's damage is in some doorbell");
    assert!(events as f64 <= 11.0 * secs.max(1.0) + 1.0, "{events} doorbells in {secs:.2} s");
    assert!(events >= 5, "{events}");
}

#[test]
fn input_goes_to_the_session_only_with_control() {
    let surfaces = leak();
    let (hub, rx, log) = hub(surfaces);
    let key = SurfaceKey::Session { machine: "dev".into(), session: "s-3".into() };
    surfaces.update(&key, 100, 100, Rect::new(0, 0, 100, 100), |_, _| {});
    let (view, _) = viewer(&hub, session_req("s-3"));
    // The feeder's connection; this test doesn't feed it.
    let (feeder, _) = agent(&rx);
    let mut w = view.get_ref().try_clone().unwrap();
    w.write_all(b"{\"ev\":\"pointer\",\"x\":5,\"y\":5}\n").unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(100)).is_err(), "no control, no input stream");

    w.write_all(b"{\"ev\":\"control\",\"on\":true}\n{\"ev\":\"key\",\"code\":4096,\"down\":true}\n{\"ev\":\"pointer\",\"x\":5, \"y\":6}\n").unwrap();
    let (mut input, request) = agent(&rx);
    assert_eq!(request, json!({"v": 1, "session": "s-3", "verb": "input.stream", "args": {}}));
    input.get_ref().write_all(b"{\"ok\":true,\"payload_len\":0}\n").unwrap();
    assert_eq!(read_line(&mut input).unwrap().unwrap(), "{\"ev\":\"pointer\",\"x\":5, \"y\":6}", "verbatim, after the invalid line was dropped");
    w.write_all(b"{\"ev\":\"button\",\"button\":272,\"down\":true}\n").unwrap();
    assert_eq!(read_line(&mut input).unwrap().unwrap(), "{\"ev\":\"button\",\"button\":272,\"down\":true}");

    // Hanging up releases what the viewer held, and the pipe closes.
    drop(w);
    drop(view);
    assert_eq!(read_line(&mut input).unwrap().unwrap(), "{\"ev\":\"release_all\"}");
    assert_eq!(read_line(&mut input).unwrap(), None);
    let deadline = Instant::now() + Duration::from_secs(5);
    while log.lock().unwrap().len() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(*log.lock().unwrap(), vec![("dev".into(), Some("s-3".into()), true), ("dev".into(), Some("s-3".into()), false)]);
    drop(feeder);
}

#[test]
fn control_off_releases_and_stops_forwarding() {
    let surfaces = leak();
    let (hub, rx, log) = hub(surfaces);
    let key = SurfaceKey::Session { machine: "dev".into(), session: "s-4".into() };
    surfaces.update(&key, 10, 10, Rect::new(0, 0, 10, 10), |_, _| {});
    let (view, _) = viewer(&hub, session_req("s-4"));
    let _feeder = agent(&rx);
    let mut w = view.get_ref().try_clone().unwrap();
    w.write_all(b"{\"ev\":\"control\",\"on\":true}\n{\"ev\":\"key\",\"code\":30,\"down\":true}\n").unwrap();
    let (mut input, _) = agent(&rx);
    input.get_ref().write_all(b"{\"ok\":true}\n").unwrap();
    assert_eq!(read_line(&mut input).unwrap().unwrap(), "{\"ev\":\"key\",\"code\":30,\"down\":true}");
    w.write_all(b"{\"ev\":\"control\",\"on\":false}\n{\"ev\":\"key\",\"code\":31,\"down\":true}\n").unwrap();
    assert_eq!(read_line(&mut input).unwrap().unwrap(), "{\"ev\":\"release_all\"}");
    assert_eq!(read_line(&mut input).unwrap(), None, "nobody has control: the pipe closes");
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(log.lock().unwrap().iter().map(|l| l.2).collect::<Vec<_>>(), [true, false]);
}

/// A scanout viewer whose input session is `s-old`, with `desktop` as the
/// session the records say is on scanout 0 now.
fn scanout_viewer(desktop: Option<&'static str>) -> (BufReader<UnixStream>, UnixStream, mpsc::Receiver<UnixStream>, Log) {
    let surfaces = leak();
    let (tx, rx) = mpsc::channel();
    let log: Log = Arc::default();
    let l = log.clone();
    let write = Box::new(move |m: &str, s: Option<&str>, on: bool| l.lock().unwrap().push((m.into(), s.map(str::to_string), on)));
    let lookup = Box::new(move |m: &str, n: u32| desktop.filter(|_| m == "dev" && n == 0).map(str::to_string));
    let hub = Arc::new(Hub::new(surfaces, Box::new(Fake(Mutex::new(tx))), write).desktops(lookup));
    surfaces.update(&SurfaceKey::Scanout(0), 10, 10, Rect::new(0, 0, 10, 10), |_, _| {});
    let (view, _) = viewer(&hub, DisplayReq { scanout: Some(0), ..session_req("s-old") });
    let mut w = view.get_ref().try_clone().unwrap();
    w.write_all(b"{\"ev\":\"control\",\"on\":true}\n{\"ev\":\"key\",\"code\":30,\"down\":true}\n").unwrap();
    let (gone, request) = agent(&rx);
    assert_eq!(request["session"], "s-old");
    gone.get_ref().write_all(b"{\"ok\":false,\"error\":\"no session s-old in dev\"}\n").unwrap();
    (view, w, rx, log)
}

#[test]
fn scanout_input_follows_the_desktop_session_now_on_it() {
    let (_view, mut w, rx, log) = scanout_viewer(Some("s-new"));
    let (mut input, request) = agent(&rx);
    assert_eq!(request, json!({"v": 1, "session": "s-new", "verb": "input.stream", "args": {}}));
    input.get_ref().write_all(b"{\"ok\":true}\n").unwrap();
    assert_eq!(read_line(&mut input).unwrap().unwrap(), "{\"ev\":\"key\",\"code\":30,\"down\":true}", "the line that found it gone");
    w.write_all(b"{\"ev\":\"key\",\"code\":30,\"down\":false}\n").unwrap();
    assert_eq!(read_line(&mut input).unwrap().unwrap(), "{\"ev\":\"key\",\"code\":30,\"down\":false}");
    let sessions: Vec<_> = log.lock().unwrap().iter().map(|l| (l.1.clone().unwrap(), l.2)).collect();
    assert_eq!(sessions, [("s-old".into(), true), ("s-old".into(), false), ("s-new".into(), true)]);
}

#[test]
fn scanout_input_waits_for_a_desktop_session() {
    let (mut view, mut w, rx, log) = scanout_viewer(None);
    // The stream goes on, and input is dropped without asking the guest.
    w.write_all(b"{\"ev\":\"key\",\"code\":30,\"down\":false}\n").unwrap();
    assert!(rx.recv_timeout(Duration::from_millis(200)).is_err(), "no input stream while there's no session");
    assert_eq!(next(&mut view)["ev"], "frame");
    assert_eq!(next(&mut view)["ev"], "keepalive", "the picture's stream is still open");
    assert_eq!(log.lock().unwrap().iter().map(|l| l.2).collect::<Vec<_>>(), [true, false]);
}
