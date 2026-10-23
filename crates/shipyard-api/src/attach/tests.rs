use super::*;

/// An attach waits in the table until taken, once; one whose wait has
/// ended is dropped as others come (whichever container they waited for),
/// and skipped at the handover.
#[test]
fn pending_attaches_are_taken_once() {
    let pending = Pending::default();
    let keys = DetachKeys::default_keys();
    let first = pending.register("abc", &keys);
    let gone = pending.register("abc", &keys);
    let taken = pending.take("abc");
    assert_eq!(taken.len(), 2);
    assert!(pending.take("abc").is_empty(), "taken once");
    drop((first, gone, taken));

    let ended = pending.register("abc", &keys);
    drop(ended);
    let _waiting = pending.register("def", &keys);
    assert!(
        pending.take("abc").is_empty(),
        "an ended wait is dropped as others come"
    );
    assert_eq!(pending.take("def").len(), 1);
}

/// F116: waiting attaches are welcomed in parallel, at most
/// `WELCOMES_AT_ONCE` (the monitor's bound), each handed its answer: 16 of
/// 300 ms take one welcome's time, not 16 one after another; those past
/// the bound are let go at once (their sender dropped).
#[test]
fn waiting_attaches_are_welcomed_in_parallel_within_a_bound() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let pending = Pending::default();
    let keys = DetachKeys::default_keys();
    let receivers: Vec<_> = (0..20).map(|_| pending.register("abc", &keys)).collect();
    let (now, most) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let began = std::time::Instant::now();
    hand_over_with(pending.take("abc"), &|_| {
        let at = now.fetch_add(1, Ordering::SeqCst) + 1;
        most.fetch_max(at, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(300));
        now.fetch_sub(1, Ordering::SeqCst);
        Err(JoinError::Welcome(AttachError::NotLetIn))
    });
    let took = began.elapsed();
    assert!(most.load(Ordering::SeqCst) <= WELCOMES_AT_ONCE, "{most:?}");
    assert!(most.load(Ordering::SeqCst) > 1, "not in parallel");
    assert!(took < std::time::Duration::from_secs(2), "{took:?}");
    let (answered, let_go): (Vec<_>, Vec<_>) = receivers
        .into_iter()
        .map(|mut receiver| receiver.try_recv())
        .partition(|answer| matches!(answer, Ok(Err(_))));
    assert_eq!(
        answered.len(),
        WELCOMES_AT_ONCE,
        "each welcomed handed its answer"
    );
    assert!(
        let_go.iter().all(|answer| matches!(
            answer,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed)
        )),
        "the rest let go"
    );
}
