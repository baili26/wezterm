//! End-to-end checks for the ZMODEM drivers.
//!
//! The interesting failure modes here are not "does it compile" but ordering
//! bugs in the pump loop: stopping on an event before the trailing bytes are
//! written, dropping the part of a chunk the engine did not consume, or
//! spinning without draining the socket.  Those only show up over a real
//! transport, so these tests push a multi-megabyte payload through a pair of
//! unidirectional socketpairs in both directions.
//!
//! (A single `UnixStream::pair` cannot be used: it is bidirectional, so each
//! side would read back its own writes.  We need one socket per direction.)

use mux::zmodem::detector::Mode;
use mux::zmodem::sink;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use zmodem2::{Action, Event, FileInfo, Position, Receiver, Sender};

fn payload(len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut s: u64 = 0x9e3779b97f4a7c15;
    for b in out.iter_mut() {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (s >> 33) as u8;
    }
    // Make sure the ZDLE escape path is exercised.
    for (idx, b) in out.iter_mut().enumerate() {
        if idx % 997 == 0 {
            *b = 0x18;
        }
    }
    out
}

fn pair() -> (UnixStream, UnixStream) {
    let (a, b) = UnixStream::pair().unwrap();
    a.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    b.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
    (a, b)
}

/// Run a `zmodem2::Sender` in a background thread.
///
/// `to_peer` is where protocol bytes are written; `from_peer` is what the
/// receiver sends back.
fn spawn_sender(
    mut to_peer: UnixStream,
    mut from_peer: UnixStream,
    data: Vec<u8>,
    name: &'static [u8],
) -> std::thread::JoinHandle<anyhow::Result<()>> {
    std::thread::spawn(move || {
        let mut sender = Sender::new()?;
        sender.set_streaming_window(usize::MAX);
        sender.start_file(FileInfo::new(name, Some(Position::new(data.len() as u32))))?;
        sender.finish()?;

        let mut pending = Vec::new();
        let mut buf = [0u8; 65536];
        let mut done = false;
        let deadline = Instant::now() + Duration::from_secs(120);

        loop {
            if Instant::now() > deadline {
                anyhow::bail!("sender timed out");
            }
            match from_peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => pending.extend_from_slice(&buf[..n]),
                Err(_) => {}
            }

            loop {
                let consumed = sender.submit_wire(&pending)?;
                if consumed == 0 {
                    break;
                }
                pending.drain(..consumed);
            }

            let mut idle = true;
            loop {
                match sender.poll() {
                    Action::WriteWire(bytes) => {
                        idle = false;
                        let n = bytes.len();
                        to_peer.write_all(bytes)?;
                        sender.wire_written(n);
                    }
                    Action::ReadFile { offset, max_len } => {
                        idle = false;
                        let off = offset.get() as usize;
                        let end = (off + max_len).min(data.len());
                        sender.submit_file(&data[off..end])?;
                    }
                    Action::Event(Event::SessionCompleted) => done = true,
                    Action::Event(Event::Aborted) => anyhow::bail!("sender aborted"),
                    Action::Event(_) => {}
                    Action::Idle => break,
                    _ => break,
                }
            }
            if done && idle {
                break;
            }
            if idle {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    })
}

fn run_receiver(
    mut from_peer: UnixStream,
    mut to_peer: UnixStream,
    dir: &std::path::Path,
) -> anyhow::Result<(Vec<u8>, Vec<std::path::PathBuf>)> {
    let mut receiver = Receiver::with_flow_control(0, true)?;
    receiver.set_manual_file_accept(true);

    let mut pending = Vec::new();
    let mut buf = [0u8; 65536];
    let mut file = None::<std::fs::File>;
    let mut collected = Vec::new();
    let mut paths = Vec::new();
    let mut done = false;
    let deadline = Instant::now() + Duration::from_secs(120);

    loop {
        if Instant::now() > deadline {
            anyhow::bail!("receiver timed out");
        }
        match from_peer.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => pending.extend_from_slice(&buf[..n]),
            Err(_) => {}
        }

        loop {
            let consumed = receiver.submit_wire(&pending)?;
            if consumed == 0 {
                break;
            }
            pending.drain(..consumed);
        }

        let mut idle = true;
        loop {
            match receiver.poll() {
                Action::WriteWire(bytes) => {
                    idle = false;
                    let n = bytes.len();
                    to_peer.write_all(bytes)?;
                    receiver.wire_written(n);
                }
                Action::WriteFile(bytes) => {
                    idle = false;
                    let n = bytes.len();
                    collected.extend_from_slice(bytes);
                    if let Some(f) = file.as_mut() {
                        f.write_all(bytes)?;
                    }
                    receiver.file_written(n)?;
                }
                Action::Event(Event::FileStarted(info)) => {
                    idle = false;
                    let name = String::from_utf8_lossy(info.name).into_owned();
                    let (path, f) = sink::create_file(dir, Some(&name), false)?;
                    paths.push(path);
                    file = Some(f);
                    receiver.accept_file_at(0)?;
                }
                Action::Event(Event::FileCompleted) => {
                    idle = false;
                    if let Some(mut f) = file.take() {
                        f.flush()?;
                    }
                }
                Action::Event(Event::SessionCompleted) => done = true,
                Action::Event(Event::Aborted) => anyhow::bail!("receiver aborted"),
                Action::Event(_) => {}
                Action::Idle => break,
                _ => break,
            }
        }
        if done && idle {
            break;
        }
        if idle {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    Ok((collected, paths))
}

#[test]
fn round_trip_large_payload() {
    // sender -> receiver, and receiver -> sender
    let (s2r_w, s2r_r) = pair();
    let (r2s_w, r2s_r) = pair();

    let data = payload(3 * 1024 * 1024 + 12345);
    let expected = data.clone();
    let sender = spawn_sender(s2r_w, r2s_r, data, b"payload.bin");

    let dir = tempfile::tempdir().unwrap();
    let (got, paths) = run_receiver(s2r_r, r2s_w, dir.path()).unwrap();

    sender.join().unwrap().unwrap();

    assert_eq!(got.len(), expected.len());
    assert!(got == expected, "received bytes differ from what was sent");
    assert_eq!(paths.len(), 1);
    let on_disk = std::fs::read(&paths[0]).unwrap();
    assert_eq!(on_disk, expected);
}

#[test]
fn receiver_sanitizes_remote_names() {
    let (s2r_w, s2r_r) = pair();
    let (r2s_w, r2s_r) = pair();

    let data = payload(256 * 1024);
    let expected = data.clone();
    // A name that tries to escape the download directory.
    let sender = spawn_sender(s2r_w, r2s_r, data, b"../../evil.bin");

    let dir = tempfile::tempdir().unwrap();
    let (got, paths) = run_receiver(s2r_r, r2s_w, dir.path()).unwrap();
    sender.join().unwrap().unwrap();

    assert_eq!(got, expected);
    assert_eq!(paths.len(), 1);
    assert_eq!(paths[0], dir.path().join("evil.bin"));
    assert!(!dir.path().parent().unwrap().join("evil.bin").exists());
}

#[test]
fn detector_state_survives_the_real_handshake() {
    // The exact bytes lrzsz emits, split the way a pty would deliver them.
    let mut stream = b"rz\r".to_vec();
    stream.extend_from_slice(b"**\x18B00000000000000");
    stream.extend_from_slice(b"\r\n\x8a\x11");

    let mut detector = mux::zmodem::ZmodemDetector::new();
    let mut passthrough = Vec::new();
    let mut mode = None;
    for chunk in stream.chunks(3) {
        let feed = detector.feed(chunk);
        passthrough.extend_from_slice(&feed.passthrough);
        if feed.trigger.is_some() {
            mode = detector.take_trigger();
            break;
        }
    }
    assert_eq!(mode, Some(Mode::Download));
    assert_eq!(passthrough, b"rz\r");
}
