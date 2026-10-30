use std::io::Write as _;

use super::*;

/// A stat in Docker's header: base64 of its JSON, its mode Go's.
#[test]
fn a_stat_is_docker_s_header() {
    let stat = PathStat {
        name: "dir".to_owned(),
        size: 4096,
        mode: 0o040_755,
        mtime_secs: 1_760_000_000,
        mtime_nanos: 5,
        link_target: String::new(),
    };
    let header = stat_header(&stat).unwrap();
    let json = base64::engine::general_purpose::STANDARD
        .decode(header.as_bytes())
        .unwrap();
    let shown: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        shown,
        serde_json::json!({"name": "dir", "size": 4096, "mode": (1_u32 << 31) | 0o755,
                           "mtime": "2025-10-09T08:53:20.000000005Z", "linkTarget": ""})
    );
}

fn never() -> Gone {
    Gone::new(|| false)
}

/// A pump's progress no test bumps (its frames all sent already).
fn quiet() -> Arc<reply::Progress> {
    Arc::new(reply::Progress::default())
}

/// A body whose frames come one at a time is read as each comes, not a
/// look's wait later: 400 frames, each sent 2 ms after the last (so the
/// reader finds the body empty and waits for each one), are read within a
/// moment of the last one's sending, not the 20 s that a 50 ms look a frame
/// cost (11.7's measure: a PUT read 16 frames a look). Measured against the
/// pump's own time: a loaded host's sleeps last longer (a CI runner's 400
/// took 6 s).
#[test]
fn a_body_is_read_as_each_frame_comes() {
    let (frames, receiver) = mpsc::channel(1);
    let progress = quiet();
    let pumped = Arc::clone(&progress);
    let began = Instant::now();
    let pump = std::thread::spawn(move || {
        let _ended = reply::BumpOnDrop(Arc::clone(&pumped));
        for _ in 0..400 {
            let mut frame = Ok(Bytes::from_static(b"x"));
            loop {
                match frames.try_send(frame) {
                    Ok(()) => break,
                    Err(mpsc::error::TrySendError::Full(back)) => {
                        frame = back;
                        std::thread::yield_now();
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return began.elapsed(),
                }
            }
            pumped.bump();
            // Paced: the reader waits for the next frame, every time.
            std::thread::sleep(Duration::from_millis(2));
        }
        began.elapsed()
    });
    let mut read = Vec::new();
    BodyReader::new(receiver, progress, never())
        .read_to_end(&mut read)
        .unwrap();
    let took = began.elapsed();
    let pumped_for = pump.join().unwrap();
    assert_eq!(read.len(), 400);
    assert!(
        took < pumped_for + Duration::from_secs(2),
        "took {took:?}, the frames {pumped_for:?}"
    );
}

/// A body's frames read in order; its end, the reader's; an error in it,
/// the reader's error, again on every later read (never an end: a
/// truncated archive mustn't pass).
#[test]
fn a_body_is_read_as_it_comes() {
    let (frames, receiver) = mpsc::channel(4);
    frames.try_send(Ok(Bytes::from_static(b"hel"))).unwrap();
    frames.try_send(Ok(Bytes::from_static(b""))).unwrap();
    frames.try_send(Ok(Bytes::from_static(b"lo"))).unwrap();
    drop(frames);
    let mut read = String::new();
    BodyReader::new(receiver, quiet(), never())
        .read_to_string(&mut read)
        .unwrap();
    assert_eq!(read, "hello");

    let (frames, receiver) = mpsc::channel(4);
    frames.try_send(Ok(Bytes::from_static(b"part"))).unwrap();
    frames
        .try_send(Err(std::io::Error::other("the client went")))
        .unwrap();
    drop(frames);
    let mut reader = BodyReader::new(receiver, quiet(), never());
    let mut read = Vec::new();
    assert!(reader.read_to_end(&mut read).is_err());
    assert!(
        reader.read(&mut [0; 8]).is_err(),
        "still an error, not an end"
    );
}

/// A body that fails before its first bytes is an error, never an empty
/// archive.
#[test]
fn a_body_failing_at_once_is_no_archive() {
    let (frames, receiver) = mpsc::channel(4);
    frames
        .try_send(Err(std::io::Error::other("the client went")))
        .unwrap();
    drop(frames);
    assert!(decompressed(BodyReader::new(receiver, quiet(), never())).is_err());
}

/// A service stopping lets a body that has nothing yet go, with an error.
#[test]
fn a_waiting_body_is_let_go_when_the_service_stops() {
    let (_frames, receiver) = mpsc::channel::<std::io::Result<Bytes>>(4);
    let mut reader = BodyReader::new(receiver, quiet(), Gone::new(|| true));
    assert!(reader.read(&mut [0; 8]).is_err());
}

/// The compression an archive's first bytes say.
#[test]
fn compression_is_known_by_its_magic() {
    assert_eq!(compression(b"\x1f\x8b\x08rest"), Compression::Gzip);
    assert_eq!(compression(b"BZh91AY"), Compression::Bzip2);
    assert_eq!(compression(b"\xfd7zXZ\x00rest"), Compression::Xz);
    assert_eq!(compression(b"file.txt\0\0\0"), Compression::None);
    assert_eq!(compression(b"\x1f"), Compression::None, "too short to say");
    assert_eq!(compression(b""), Compression::None);
}

/// A gzip archive is read decoded, its magic found however the body's
/// frames split it; a plain one as it is; bzip2 refused.
#[test]
fn a_gzip_body_is_decoded() {
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gzip.write_all(b"the archive").unwrap();
    let gzip = gzip.finish().unwrap();
    let (frames, receiver) = mpsc::channel(gzip.len() + 1);
    for byte in &gzip {
        frames.try_send(Ok(Bytes::from(vec![*byte]))).unwrap();
    }
    drop(frames);
    let mut read = String::new();
    let Ok(Ok(mut decoded)) = decompressed(BodyReader::new(receiver, quiet(), never())) else {
        panic!("not read as gzip");
    };
    decoded.read_to_string(&mut read).unwrap();
    assert_eq!(read, "the archive");
    let mut plain = String::new();
    let Ok(Ok(mut as_is)) = decompressed(std::io::Cursor::new(b"plain".to_vec())) else {
        panic!("not read as it is");
    };
    as_is.read_to_string(&mut plain).unwrap();
    assert_eq!(plain, "plain");
    assert!(matches!(
        decompressed(std::io::Cursor::new(b"BZh9".to_vec())),
        Ok(Err(Refusal::Compressed("bzip2")))
    ));
}
