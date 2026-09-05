#![allow(clippy::expect_used)]

use super::*;
use zeppelin_embed::lifecycle::{Deadline, ManualMonotonicClock};

std::thread_local! {
    pub(super) static QUEUE_FULL_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn tokens() -> TokenBatch {
    TokenBatch {
        token_ids: vec![1],
        attention_mask: vec![1.0],
        rows: 1,
        tokens_per_row: 1,
    }
}

fn run_fake_embedding_commands(
    receiver: mpsc::Receiver<RuntimeCommand>,
    closed: &AtomicBool,
    mut embed: impl FnMut(TowerRole, &TokenBatch) -> Result<EmbeddingBatch, RuntimeError>,
) {
    run_embedding_commands(receiver, closed, |role, tokens, _| {
        embed(role, tokens).map_err(embedding_error)
    });
}

// The command is already in the real bounded channel before logical time moves.
// Starting the owner afterward establishes ordering without timing assumptions.
fn queued_after_stop(control: QueryControl, stop: impl FnOnce()) -> (bool, usize) {
    let (sender, receiver) = mpsc::sync_channel(2);
    let (reply, receive) = mpsc::sync_channel(0);
    sender
        .send(RuntimeCommand::Embed {
            role: TowerRole::Query,
            tokens: tokens(),
            inject_panic: false,
            queued: None,
            control: Some(control),
            reply,
        })
        .expect("queued before stopping");
    stop();
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&calls);
    let worker = std::thread::spawn(move || {
        run_fake_embedding_commands(receiver, &AtomicBool::new(false), |_, _| {
            observed.fetch_add(1, Ordering::SeqCst);
            EmbeddingBatch::new(vec![1.0, 0.0], 1, 2)
        });
    });
    let failed = receive
        .recv_timeout(Duration::from_secs(10))
        .expect("watchdog: queued command must complete")
        .is_err();
    sender.send(RuntimeCommand::Close).expect("close");
    worker.join().expect("worker joined before assertions");
    (failed, calls.load(Ordering::SeqCst))
}

#[test]
fn astra_18_text_deadline_includes_embedding_queue_wait() {
    let clock = Arc::new(ManualMonotonicClock::new());
    let deadline =
        Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone()).expect("deadline");
    let (failed, calls) = queued_after_stop(QueryControl::Deadline(deadline), || {
        clock.advance(Duration::from_secs(2));
    });
    assert_eq!(
        calls, 0,
        "expired queued command must not enter native model"
    );
    assert!(failed, "deadline expiration must return a typed error");
}

#[test]
fn astra_18_canceled_queued_embedding_is_not_executed() {
    let token = CancelToken::new();
    let (failed, calls) = queued_after_stop(QueryControl::Cancel(token.clone()), || token.cancel());
    assert_eq!(
        calls, 0,
        "canceled queued command must not enter native model"
    );
    assert!(failed, "cancellation must return a typed error");
}

fn submit(
    sender: &mpsc::SyncSender<RuntimeCommand>,
    control: Option<QueryControl>,
    inject_panic: bool,
) -> mpsc::Receiver<Result<RuntimeEmbedding, TextError>> {
    let (reply, receive) = mpsc::sync_channel(0);
    sender
        .send(RuntimeCommand::Embed {
            role: TowerRole::Query,
            tokens: tokens(),
            inject_panic,
            queued: None,
            control,
            reply,
        })
        .expect("submit ordered command");
    receive
}

fn fake_client(
    embed: impl FnMut(TowerRole, &TokenBatch) -> Result<EmbeddingBatch, RuntimeError> + Send + 'static,
) -> RuntimeClient {
    let (sender, receiver) = mpsc::sync_channel(2);
    let closed = Arc::new(AtomicBool::new(false));
    let worker_closed = Arc::clone(&closed);
    let thread =
        std::thread::spawn(move || run_fake_embedding_commands(receiver, &worker_closed, embed));
    RuntimeClient {
        query_backend: QueryBackend {
            runtime: crate::runtime::RuntimeIdentity {
                name: "deterministic-test",
                gpu: false,
            },
            requested_compute_units: zeppelin_embed::epoch::ComputeUnits::Cpu,
            observed_compute_units: None,
            sequence_length: None,
        },
        sender,
        thread: Mutex::new(Some(thread)),
        closed,
    }
}

#[test]
fn astra_18_saturated_runtime_admission_uses_original_deadline() {
    struct FullQueueClock {
        base: Instant,
        expired: AtomicBool,
    }
    impl zeppelin_embed::lifecycle::MonotonicClock for FullQueueClock {
        fn now(&self) -> Instant {
            if QUEUE_FULL_PROBES.with(std::cell::Cell::get) > 0 {
                self.expired.store(true, Ordering::SeqCst);
            }
            if self.expired.load(Ordering::SeqCst) {
                self.base + Duration::from_secs(2)
            } else {
                self.base
            }
        }
    }
    let (entered, entering) = mpsc::sync_channel(0);
    let (release, released) = mpsc::sync_channel(0);
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = Arc::clone(&calls);
    let client = fake_client(move |_, _| {
        if worker_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            entered.send(()).expect("native entry");
            released
                .recv_timeout(Duration::from_secs(10))
                .expect("native release watchdog");
        }
        EmbeddingBatch::new(vec![1.0, 0.0], 1, 2)
    });
    let running = submit(&client.sender, None, false);
    entering
        .recv_timeout(Duration::from_secs(10))
        .expect("running before queue fill");
    let queued = CancelToken::new();
    let first = submit(
        &client.sender,
        Some(QueryControl::Cancel(queued.clone())),
        false,
    );
    let second = submit(
        &client.sender,
        Some(QueryControl::Cancel(queued.clone())),
        false,
    );
    QUEUE_FULL_PROBES.with(|value| value.set(0));
    let clock = Arc::new(FullQueueClock {
        base: Instant::now(),
        expired: AtomicBool::new(false),
    });
    let deadline =
        Deadline::after_with_test_clock(Duration::from_secs(1), clock).expect("deadline");
    let before_release = std::thread::scope(|scope| {
        let (send, receive) = mpsc::sync_channel(1);
        let query_client = &client;
        let query = scope.spawn(move || {
            QUEUE_FULL_PROBES.with(|value| value.set(0));
            let result = query_client.embed_with_timing_controlled(
                TowerRole::Query,
                tokens(),
                false,
                Some(QueryControl::Deadline(deadline)),
            );
            let full = QUEUE_FULL_PROBES.with(std::cell::Cell::get);
            send.send((result, full)).expect("query result");
        });
        let before_release = receive.recv_timeout(Duration::from_secs(2));
        queued.cancel();
        drop(first);
        drop(second);
        release.send(()).expect("finish native call");
        let running = running
            .recv_timeout(Duration::from_secs(10))
            .expect("running reply");
        query
            .join()
            .expect("caller joined even after a failed watchdog");
        client
            .close()
            .expect("close drains canceled queue and joins");
        assert!(running.is_ok());
        before_release
    });
    let (result, full) =
        before_release.expect("expired caller must return before capacity is available");
    assert!(matches!(
        result,
        Err(TextError::Query(
            zeppelin_embed::lifecycle::QueryError::Timeout { partial: false }
        ))
    ));
    assert!(
        full > 0,
        "deadline expires only after the real bounded channel refuses admission"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "queued and refused requests never evaluate"
    );
    println!(
        "saturated admission full_probes={full}, native_calls=1, canceled_queue=2, joined=true"
    );
}

#[test]
fn astra_18_runtime_close_cancels_waiter_and_joins_native_completion() {
    let (entered, entering) = mpsc::sync_channel(0);
    let (release, released) = mpsc::sync_channel(0);
    let completed = Arc::new(AtomicBool::new(false));
    let worker_completed = Arc::clone(&completed);
    let client = fake_client(move |_, _| {
        entered.send(()).expect("native entry");
        released
            .recv_timeout(Duration::from_secs(10))
            .expect("native release watchdog");
        worker_completed.store(true, Ordering::Release);
        EmbeddingBatch::new(vec![1.0, 0.0], 1, 2)
    });
    let (query_result, close_before_release, close_result) = std::thread::scope(|scope| {
        let (send_query, receive_query) = mpsc::sync_channel(1);
        let query_client = &client;
        let query = scope.spawn(move || {
            let result = query_client.embed_with_timing_controlled(
                TowerRole::Query,
                tokens(),
                false,
                Some(QueryControl::Cancel(CancelToken::new())),
            );
            send_query.send(result).expect("caller result");
        });
        entering
            .recv_timeout(Duration::from_secs(10))
            .expect("native entered before close");
        let (send_close, receive_close) = mpsc::sync_channel(1);
        let close_client = &client;
        let close = scope.spawn(move || {
            send_close.send(close_client.close()).expect("close result");
        });
        let query_result = receive_query.recv_timeout(Duration::from_secs(2));
        let close_before_release = receive_close.try_recv();
        let close_waited = matches!(close_before_release, Err(mpsc::TryRecvError::Empty));
        release
            .send(())
            .expect("native completion after canceled caller");
        query.join().expect("caller joined");
        close.join().expect("close joined");
        let close_result = match close_before_release {
            Ok(result) => result,
            Err(_) => receive_close
                .recv_timeout(Duration::from_secs(2))
                .expect("close completion"),
        };
        (query_result, close_waited, close_result)
    });
    assert!(matches!(
        query_result,
        Ok(Err(TextError::Query(
            zeppelin_embed::lifecycle::QueryError::ReadCancelled { partial: false }
        )))
    ));
    assert!(
        close_before_release,
        "close must retain and join the running native worker"
    );
    assert!(close_result.is_ok());
    assert!(completed.load(Ordering::Acquire));
    assert!(client.thread.lock().expect("join slot").is_none());
    println!(
        "close precedence=ReadCancelled, caller returned before native completion, close joined=true"
    );
}

#[test]
fn astra_18_native_completion_preserves_control_error_precedence() {
    use zeppelin_embed::lifecycle::QueryError;
    // The fake native call has entered before stop; it returns only after stop.
    // Exercise successful output, an ordinary runtime error and an unwind.
    for stop in ["cancel", "deadline", "close"] {
        for completion in ["success", "error", "panic"] {
            let (sender, receiver) = mpsc::sync_channel(2);
            let (entered, entering) = mpsc::sync_channel(0);
            let (release, released) = mpsc::sync_channel(0);
            let closed = Arc::new(AtomicBool::new(false));
            let worker_closed = Arc::clone(&closed);
            let calls = Arc::new(AtomicUsize::new(0));
            let worker_calls = Arc::clone(&calls);
            let token = CancelToken::new();
            let clock = Arc::new(ManualMonotonicClock::new());
            let control = if stop == "deadline" {
                QueryControl::Deadline(
                    Deadline::after_with_test_clock(Duration::from_secs(1), clock.clone())
                        .expect("deadline"),
                )
            } else {
                QueryControl::Cancel(token.clone())
            };
            let receive = submit(&sender, Some(control), false);
            let worker = std::thread::spawn(move || {
                run_fake_embedding_commands(receiver, &worker_closed, |_, _| {
                    worker_calls.fetch_add(1, Ordering::SeqCst);
                    entered.send(()).expect("native entry");
                    released
                        .recv_timeout(Duration::from_secs(10))
                        .expect("native release watchdog");
                    match completion {
                        "panic" => std::panic::resume_unwind(Box::new("completion panic")),
                        "error" => Err(RuntimeError::Shape("completion error".to_owned())),
                        _ => EmbeddingBatch::new(vec![1.0, 0.0], 1, 2),
                    }
                });
            });
            entering
                .recv_timeout(Duration::from_secs(10))
                .expect("entry watchdog");
            match stop {
                "deadline" => clock.advance(Duration::from_secs(2)),
                "close" => {
                    token.cancel(); // Close must win even when caller also stopped.
                    closed.store(true, Ordering::Release);
                }
                _ => token.cancel(),
            }
            release.send(()).expect("complete after stop");
            let result = receive
                .recv_timeout(Duration::from_secs(10))
                .expect("reply watchdog");
            sender.send(RuntimeCommand::Close).expect("close command");
            worker.join().expect("join before assertions");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert!(
                match (stop, result) {
                    ("cancel", Err(TextError::Query(QueryError::Cancelled { partial: false }))) =>
                        true,
                    ("deadline", Err(TextError::Query(QueryError::Timeout { partial: false }))) =>
                        true,
                    (
                        "close",
                        Err(TextError::Query(QueryError::ReadCancelled { partial: false })),
                    ) => true,
                    _ => false,
                },
                "stop={stop}, completion={completion}"
            );
            println!("completion control={stop}, native={completion}, calls=1, joined=true");
        }
    }
}

#[test]
fn astra_18_query_panic_is_typed_and_worker_accepts_next_command() {
    let (sender, receiver) = mpsc::sync_channel(2);
    let calls = Arc::new(AtomicUsize::new(0));
    let worker_calls = Arc::clone(&calls);
    let worker = std::thread::spawn(move || {
        run_fake_embedding_commands(receiver, &AtomicBool::new(false), |_, _| {
            worker_calls.fetch_add(1, Ordering::SeqCst);
            EmbeddingBatch::new(vec![1.0, 0.0], 1, 2)
        });
    });
    let planted = submit(
        &sender,
        Some(QueryControl::Cancel(CancelToken::new())),
        true,
    )
    .recv_timeout(Duration::from_secs(10))
    .expect("panic reply watchdog");
    let after_plant = calls.load(Ordering::SeqCst);
    let clean = submit(
        &sender,
        Some(QueryControl::Cancel(CancelToken::new())),
        false,
    )
    .recv_timeout(Duration::from_secs(10))
    .expect("clean reply watchdog");
    sender.send(RuntimeCommand::Close).expect("close");
    worker.join().expect("join before assertions");
    assert!(matches!(
        planted,
        Err(TextError::Pipeline {
            stage: "embed worker",
            ..
        })
    ));
    assert_eq!(
        after_plant, 0,
        "existing panic site fires before native evaluation"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        clean
            .expect("same worker handles clean command")
            .batch
            .values(),
        &[1.0, 0.0]
    );
    println!("fault.site.embed-worker-panic=1; clean native calls=1; joined=true");
}
