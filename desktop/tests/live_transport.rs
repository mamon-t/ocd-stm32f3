//! Headless integration harness: desktop's own `transport::Transport` against the
//! LIVE board (no GUI window). Reuses the crate's real protocol parsing code.
//!
//! Run (board must be flashed & enumerated, `c0de:cafe` → `/dev/ttyACM0`):
//!
//! ```sh
//! LIVE_PORT=/dev/ttyACM0 cargo test --release --test live_transport \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! Verdict = Tier-1 proof of the desktop↔firmware protocol path: GET_MASK answer,
//! SET_RATE answer, and a non-empty `Event::Frame` stream with monotonically
//! incrementing `seq`.

#[path = "../src/transport.rs"]
mod transport;

use std::time::{Duration, Instant};

use transport::{Ack, Event, Transport};

const BAUD: u32 = 115_200;
const EVENT_TIMEOUT: Duration = Duration::from_millis(100);
const SAMPLE_WINDOW: Duration = Duration::from_secs(4);
const GLOBAL_DEADLINE: Duration = Duration::from_secs(12);

#[test]
#[ignore = "requires a live board: set LIVE_PORT (or rely on auto-detect), board must run ocd-stm32f3"]
fn live_protocol_round_trip() {
    let port = std::env::var("LIVE_PORT").ok().filter(|p| !p.is_empty());
    eprintln!("[harness] port = {:?}", port);

    let mut t = Transport::spawn(port, BAUD);
    let deadline = Instant::now() + GLOBAL_DEADLINE;
    let mut connected = false;

    let mut got_mask = false;
    let mut got_rate = false;
    let mut got_fs = false;
    let mut frames: u64 = 0;
    let mut overrun_frames: u64 = 0;
    let mut seq_inc: u64 = 0;
    let mut last_seq: Option<u8> = None;
    let mut ch_bytes = vec![0u64; transport::NUM_CHANNELS];
    let mut ch_max = vec![0u16; transport::NUM_CHANNELS];
    let mut sampling_started: Option<Instant> = None;

    // Commands are re-sent until acked (the CDC port can flap on open; a single
    // send can land in a reconnection gap).
    let mut last_cmd = Instant::now();

    while Instant::now() < deadline {
        match t.receiver().recv_timeout(EVENT_TIMEOUT) {
            Err(_) => {}
            Ok(Event::Connected(name)) => {
                connected = true;
                eprintln!("[harness] connected: {name}");
            }
            Ok(Event::Status(s)) => eprintln!("[harness] status: {s}"),
            Ok(Event::Ack(Ack::Mask { lo, hi })) => {
                got_mask = true;
                eprintln!("[harness] ACK MASK lo={lo:#04x} hi={hi:#04x}");
            }
            Ok(Event::Ack(Ack::Rate(level))) => {
                got_rate = true;
                eprintln!("[harness] ACK RATE level={level}");
            }
            Ok(Event::Ack(Ack::SampleRate(fs))) => {
                got_fs = true;
                eprintln!("[harness] ACK SAMPLE RATE fs={fs}");
            }
            Ok(Event::Ack(other)) => eprintln!("[harness] ACK other: {other:?}"),
            Ok(Event::Frame { seq, overrun, channels }) => {
                frames += 1;
                overrun_frames += overrun as u64;
                if let Some(prev) = last_seq {
                    if seq.wrapping_sub(prev) == 1 {
                        seq_inc += 1;
                    }
                }
                last_seq = Some(seq);
                for (i, (_ch, raw)) in channels.iter().enumerate() {
                    ch_bytes[i] += 1;
                    ch_max[i] = ch_max[i].max(*raw);
                }
                sampling_started.get_or_insert_with(Instant::now);
            }
        }

        // Kick the protocol once connected, re-send until both acks arrive.
        if connected && last_cmd.elapsed() >= Duration::from_millis(300) {
            if !got_mask {
                t.get_mask();
            }
            if !got_rate {
                t.set_rate(3);
            }
            last_cmd = Instant::now();
        }

        // Stop as soon as we have a solid sampling window.
        if let Some(start) = sampling_started {
            if start.elapsed() >= SAMPLE_WINDOW {
                break;
            }
        }
    }

    // НЕ зовём t.shutdown(): его join() может заблокироваться на open()/sleep
    // ридера и подвесить тест. Процесс сам оборвёт поток при выходе.
    drop(t);

    eprintln!(
        "[harness] frames={frames} overrun={overrun_frames} seq_inc={seq_inc} mask={got_mask} rate={got_rate} fs={got_fs}"
    );
    eprintln!("[harness] ch_bytes={ch_bytes:?}");
    eprintln!("[harness] ch_max={ch_max:?}");

    assert!(connected, "no Event::Connected seen — port missing/dead?");
    assert!(got_mask, "GET_MASK never answered (Ack::Mask)");
    assert!(got_rate, "SET_RATE never answered (Ack::Rate)");
    assert!(frames > 0, "no Event::Frame collected in the sampling window");
    assert!(
        seq_inc > 0,
        "frame seq never incremented by 1 (parsing/flow issue, seq_inc={seq_inc})"
    );
    assert!(
        ch_bytes.iter().any(|&n| n > 0),
        "no channel carried any sample data"
    );
    eprintln!("[harness] VERDICT: desktop↔firmware protocol OK");
}