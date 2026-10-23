use shipyard_engine::logs::{self, Entry, Gone};
use shipyard_runtime::logs::cri::Stream;
use shipyard_runtime::logs::time::Timestamp;

use super::*;

fn at(secs: i64) -> Timestamp {
    Timestamp::from_unix(secs, 5).unwrap()
}

fn entry(stream: Stream, text: &str, complete: bool, secs: i64) -> Entry {
    Entry {
        time: at(secs),
        stream,
        text: text.as_bytes().to_vec(),
        complete,
    }
}

fn writer(terminal: bool, timestamps: bool) -> LogWriter {
    LogWriter {
        terminal,
        lines: logs::LineWriter::new(timestamps),
        streams: [true, true],
    }
}

/// Docker's header: the stream (1, 2), three zeros, the length big-endian.
#[test]
fn frames_carry_their_stream_and_length() {
    let framed = frame(Stream::Stderr, b"oops\n");
    assert_eq!(framed[..8], [2, 0, 0, 0, 0, 0, 0, 5]);
    assert_eq!(&framed[8..], b"oops\n");
    let long = vec![b'x'; 70_000];
    let framed = frame(Stream::Stdout, &long);
    assert_eq!(framed[..8], [1, 0, 0, 0, 0, 1, 0x11, 0x70]);
    assert_eq!(framed.len(), 8 + 70_000);
}

/// A line is framed with its newline; a piece without one, and its rest
/// after it, the time only at the line's start.
#[test]
fn lines_and_pieces_are_written_as_docker_writes_them() {
    let mut logs = writer(false, true);
    let line = logs.write(&entry(Stream::Stdout, "hello", true, 10));
    let time = format!("{} ", at(10));
    assert_eq!(
        line,
        frame(Stream::Stdout, format!("{time}hello\n").as_bytes())
    );
    let piece = logs.write(&entry(Stream::Stderr, "pro", false, 11));
    assert_eq!(
        piece,
        frame(Stream::Stderr, format!("{} pro", at(11)).as_bytes())
    );
    let rest = logs.write(&entry(Stream::Stderr, "mpt", true, 12));
    assert_eq!(rest, frame(Stream::Stderr, b"mpt\n"), "no time mid-line");
    let next = logs.write(&entry(Stream::Stderr, "again", true, 13));
    assert_eq!(
        next,
        frame(Stream::Stderr, format!("{} again\n", at(13)).as_bytes())
    );
}

/// With a terminal, its bytes as they are: no frame.
#[test]
fn a_terminal_s_output_is_raw() {
    let mut logs = writer(true, false);
    assert_eq!(logs.write(&entry(Stream::Stdout, "$ ", false, 1)), b"$ ");
    assert_eq!(logs.write(&entry(Stream::Stdout, "ls", true, 2)), b"ls\n");
}

/// A stream not asked for, and lines past `until`, aren't written.
#[test]
fn streams_and_until_choose_the_lines() {
    let mut logs = writer(false, false);
    logs.streams = [true, false];
    assert!(
        logs.write(&entry(Stream::Stderr, "hidden", true, 1))
            .is_empty()
    );
    assert!(
        !logs
            .write(&entry(Stream::Stdout, "shown", true, 1))
            .is_empty()
    );
}

/// A full channel: the sender waits, asking `gone` between tries; it gives
/// up as soon as `gone` says so (a client that stopped reading, the
/// service stopping), and sends as soon as there's room.
#[test]
fn a_full_channel_waits_and_asks_after_its_client() {
    let (sender, mut receiver) = reply::bare(1);
    let never = Gone::new(|| false);
    send(&sender, &never, b"first".to_vec()).unwrap();
    let asked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = std::sync::Arc::clone(&asked);
    let gone_on_the_third =
        Gone::new(move || counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2);
    // On a thread of its own, answered within a bound: a send that never
    // asked would wait for good, and fail the test, not hang it.
    let (answered, answer) = std::sync::mpsc::channel();
    let waiting = sender.clone();
    std::thread::spawn(move || {
        let _ = answered.send(send(&waiting, &gone_on_the_third, b"second".to_vec()));
    });
    let refused = answer
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("a full channel's send asks after its client")
        .unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 3);
    // Room again: sent at once.
    assert_eq!(receiver.try_recv().unwrap(), Bytes::from_static(b"first"));
    send(&sender, &never, b"third".to_vec()).unwrap();
    assert_eq!(receiver.try_recv().unwrap(), Bytes::from_static(b"third"));
    // Closed: refused without waiting.
    drop(receiver);
    let closed = send(&sender, &never, b"fourth".to_vec()).unwrap_err();
    assert_eq!(closed.kind(), std::io::ErrorKind::BrokenPipe);
    assert!(send(&sender, &never, Vec::new()).is_ok(), "nothing to send");
}

/// Docker's times; `0` and nothing are none, as Docker reads them; any
/// other text is refused.
#[test]
fn times_in_the_query_are_docker_s() {
    let query = |text: &str| crate::query::Query::parse(Some(text)).unwrap();
    assert_eq!(time(&query(""), "since").unwrap(), None);
    assert_eq!(time(&query("since=0"), "since").unwrap(), None);
    assert_eq!(time(&query("since="), "since").unwrap(), None);
    assert_eq!(
        time(&query("since=1760000000.5"), "since").unwrap(),
        Some(Timestamp::from_unix(1_760_000_000, 500_000_000).unwrap())
    );
    assert_eq!(
        time(&query("until=yesterday"), "until").unwrap_err().status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        system(Timestamp::parse("1969-01-01T00:00:00Z").unwrap()),
        std::time::SystemTime::UNIX_EPOCH,
        "before 1970: the epoch"
    );
}

/// A stream's writer goes on as soon as its client takes a chunk, not a
/// wait's worth later: 4,000 chunks through a 16-chunk channel take well
/// under the 12.5 s a 50 ms wait each time it filled cost (11.7: `logs` of
/// 50,000 lines took 153 s through the API, 0.01 s directly). Its client
/// gone mid-way, the writer's next send fails (never waits for good).
#[test]
fn a_writer_goes_on_as_its_client_reads() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (sender, body) = reply::channel();
    let writer = std::thread::spawn(move || {
        let gone = Gone::new(|| false);
        for _ in 0..4000 {
            send(&sender, &gone, b"line\n".to_vec()).unwrap();
        }
    });
    let began = std::time::Instant::now();
    let read = runtime.block_on(async move {
        use http_body_util::BodyExt as _;
        let mut body = std::pin::pin!(body);
        let mut read = 0;
        while let Some(frame) = body.as_mut().frame().await {
            read += frame.unwrap().into_data().unwrap().len();
        }
        read
    });
    let took = began.elapsed();
    writer.join().unwrap();
    assert_eq!(read, 4000 * 5);
    assert!(took < std::time::Duration::from_secs(4), "took {took:?}");

    let (sender, body) = reply::channel();
    let gone = Gone::new(|| false);
    for _ in 0..16 {
        send(&sender, &gone, b"x".to_vec()).unwrap();
    }
    let blocked = std::thread::spawn(move || send(&sender, &gone, b"x".to_vec()));
    std::thread::sleep(std::time::Duration::from_millis(100));
    drop(body);
    let failed = blocked.join().unwrap().unwrap_err();
    assert_eq!(failed.kind(), std::io::ErrorKind::BrokenPipe);
}
