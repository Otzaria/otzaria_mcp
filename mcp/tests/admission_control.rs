//! Deterministic ownership/cancellation tests around synchronous, serialized work.
//! The payload is deliberately neutral: no production library or index required.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use otzaria_mcp::admission::{Config, Error, Gate};
use tokio_util::sync::CancellationToken;

fn gate(queue_capacity: usize, timeout: Duration) -> Gate {
    Gate::new(Config {
        workers: 1,
        queue_capacity,
        queue_timeout: timeout,
    })
}

async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("condition did not become true");
}

#[tokio::test]
async fn finite_fifo_queue_does_not_create_mutex_waiting_workers() {
    let gate = gate(2, Duration::from_secs(5));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mutex = Arc::new(Mutex::new(Vec::new()));
    let first = {
        let gate = gate.clone();
        let mutex = mutex.clone();
        tokio::spawn(async move {
            gate.acquire(CancellationToken::new())
                .await
                .unwrap()
                .run(move || {
                    let mut work = mutex.lock().unwrap();
                    work.push(0);
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                })
                .await
                .unwrap();
        })
    };
    started_rx.await.unwrap();
    let mut waiting = Vec::new();
    for i in 1..=2 {
        let worker_gate = gate.clone();
        let mutex = mutex.clone();
        waiting.push(tokio::spawn(async move {
            worker_gate
                .acquire(CancellationToken::new())
                .await
                .unwrap()
                .run(move || {
                    mutex.lock().unwrap().push(i);
                })
                .await
                .unwrap();
        }));
        eventually(|| gate.metrics().queued == i).await;
    }
    assert!(matches!(
        gate.acquire(CancellationToken::new()).await,
        Err(Error::Full)
    ));
    let metrics = gate.metrics();
    assert_eq!(metrics.in_flight, 3);
    assert_eq!(metrics.blocking_tasks, 1);
    assert_eq!(metrics.running, 1);
    assert_eq!(metrics.rejected, 1);
    release_tx.send(()).unwrap();
    first.await.unwrap();
    for task in waiting {
        task.await.unwrap();
    }
    assert_eq!(*mutex.lock().unwrap(), [0, 1, 2]);
    let metrics = gate.metrics();
    assert_eq!(metrics.completed, 3);
    assert_eq!(metrics.peak_running, 1);
    assert_eq!(metrics.peak_in_flight, 3);
    assert_eq!(metrics.in_flight, 0);
    assert_eq!(metrics.queue.count, 3);
    assert_eq!(metrics.execution.count, 3);
    assert_eq!(metrics.latency.count, 3);
}

#[tokio::test]
async fn cancel_waiter_releases_queue_capacity_without_executing() {
    let gate = gate(1, Duration::from_secs(5));
    let held = gate.acquire(CancellationToken::new()).await.unwrap();
    let cancellation = CancellationToken::new();
    let waiter = {
        let gate = gate.clone();
        let cancellation = cancellation.clone();
        tokio::spawn(async move { gate.acquire(cancellation).await })
    };
    eventually(|| gate.metrics().in_flight == 2).await;
    cancellation.cancel();
    assert!(matches!(waiter.await.unwrap(), Err(Error::Cancelled)));
    assert_eq!(gate.metrics().cancelled_before_start, 1);
    assert_eq!(gate.metrics().in_flight, 1);
    drop(held);
    gate.acquire(CancellationToken::new())
        .await
        .unwrap()
        .run(|| ())
        .await
        .unwrap();
}

#[tokio::test]
async fn dropped_waiter_releases_queue_capacity() {
    let gate = gate(1, Duration::from_secs(5));
    let held = gate.acquire(CancellationToken::new()).await.unwrap();
    let waiter = {
        let gate = gate.clone();
        tokio::spawn(async move { gate.acquire(CancellationToken::new()).await })
    };
    eventually(|| gate.metrics().in_flight == 2).await;
    waiter.abort();
    assert!(matches!(waiter.await, Err(e) if e.is_cancelled()));
    assert_eq!(gate.metrics().in_flight, 1);
    assert_eq!(gate.metrics().cancelled_before_start, 1);
    drop(held);
}

#[tokio::test]
async fn queue_expiration_never_starts_stale_work() {
    let gate = gate(1, Duration::from_millis(30));
    let held = gate.acquire(CancellationToken::new()).await.unwrap();
    assert!(matches!(
        gate.acquire(CancellationToken::new()).await,
        Err(Error::Expired)
    ));
    assert_eq!(gate.metrics().expired_before_start, 1);
    assert_eq!(gate.metrics().running, 0);
    drop(held);
}

#[tokio::test]
async fn dropped_caller_does_not_release_running_worker_permit() {
    let gate = gate(0, Duration::from_secs(5));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let task = {
        let gate = gate.clone();
        tokio::spawn(async move {
            gate.acquire(CancellationToken::new())
                .await
                .unwrap()
                .run(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                })
                .await
        })
    };
    started_rx.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(matches!(
        gate.acquire(CancellationToken::new()).await,
        Err(Error::Full)
    ));
    assert_eq!(gate.metrics().running, 1);
    release_tx.send(()).unwrap();
    eventually(|| gate.metrics().in_flight == 0).await;
    assert_eq!(gate.metrics().completed_after_cancellation, 1);
    gate.acquire(CancellationToken::new())
        .await
        .unwrap()
        .run(|| ())
        .await
        .unwrap();
}

#[tokio::test]
async fn explicit_cancel_detaches_started_work_but_keeps_permits() {
    let gate = gate(0, Duration::from_secs(5));
    let token = CancellationToken::new();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let task = {
        let gate = gate.clone();
        let token = token.clone();
        tokio::spawn(async move {
            gate.acquire(token)
                .await
                .unwrap()
                .run(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    42
                })
                .await
        })
    };
    started_rx.await.unwrap();
    token.cancel();
    assert!(matches!(task.await.unwrap(), Err(Error::Cancelled)));
    assert!(matches!(
        gate.acquire(CancellationToken::new()).await,
        Err(Error::Full)
    ));
    release_tx.send(()).unwrap();
    eventually(|| gate.metrics().in_flight == 0).await;
    assert_eq!(gate.metrics().completed, 1);
    assert_eq!(gate.metrics().completed_after_cancellation, 1);
}

#[test]
fn dropped_caller_aborts_tokio_pool_queued_closure() {
    pool_queued_work(false);
}

#[test]
fn deadline_also_covers_tokio_pool_queue() {
    pool_queued_work(true);
}

fn pool_queued_work(expire: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let gate = gate(
            0,
            if expire {
                Duration::from_millis(30)
            } else {
                Duration::from_secs(5)
            },
        );
        let (hog_started_tx, hog_started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let hog = tokio::task::spawn_blocking(move || {
            hog_started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        hog_started_rx.await.unwrap();
        let effects = Arc::new(AtomicUsize::new(0));
        let task = {
            let gate = gate.clone();
            let effects = effects.clone();
            tokio::spawn(async move {
                gate.acquire(CancellationToken::new())
                    .await
                    .unwrap()
                    .run(move || {
                        effects.fetch_add(1, Ordering::SeqCst);
                    })
                    .await
            })
        };
        eventually(|| gate.metrics().blocking_tasks == 1).await;
        assert_eq!(gate.metrics().running, 0);
        if expire {
            assert!(matches!(task.await.unwrap(), Err(Error::Expired)));
        } else {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        // Tokio may dispose of an aborted queued closure only when a pool worker
        // becomes free. Its permit remains owned until that disposal happens.
        release_tx.send(()).unwrap();
        hog.await.unwrap();
        eventually(|| gate.metrics().in_flight == 0).await;
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(gate.metrics().completed, 0);
        if expire {
            assert_eq!(gate.metrics().expired_before_start, 1);
        } else {
            assert_eq!(gate.metrics().cancelled_before_start, 1);
        }
        gate.acquire(CancellationToken::new())
            .await
            .unwrap()
            .run(|| ())
            .await
            .unwrap();
    });
}

#[tokio::test]
async fn start_deadline_never_interrupts_a_running_operation() {
    let gate = gate(0, Duration::from_millis(50));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let task = {
        let gate = gate.clone();
        tokio::spawn(async move {
            gate.acquire(CancellationToken::new())
                .await
                .unwrap()
                .run(move || {
                    started_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    42
                })
                .await
        })
    };
    started_rx.await.unwrap();
    tokio::time::sleep(Duration::from_millis(70)).await;
    assert!(!task.is_finished());
    release_tx.send(()).unwrap();
    assert_eq!(task.await.unwrap().unwrap(), 42);
    assert_eq!(gate.metrics().expired_before_start, 0);
}

#[tokio::test]
async fn panic_releases_both_permits_and_records_failure() {
    let gate = gate(0, Duration::from_secs(5));
    let result = gate
        .acquire(CancellationToken::new())
        .await
        .unwrap()
        .run(|| panic!("test failure"))
        .await;
    assert!(matches!(result, Err(Error::Join(_))));
    assert_eq!(gate.metrics().in_flight, 0);
    assert_eq!(gate.metrics().blocking_tasks, 0);
    assert_eq!(gate.metrics().failed, 1);
    gate.acquire(CancellationToken::new())
        .await
        .unwrap()
        .run(|| ())
        .await
        .unwrap();
}

#[tokio::test]
async fn shutdown_cancels_waiters_and_prevents_new_admission() {
    let gate = gate(1, Duration::from_secs(5));
    let held = gate.acquire(CancellationToken::new()).await.unwrap();
    let task = {
        let gate = gate.clone();
        tokio::spawn(async move { gate.acquire(CancellationToken::new()).await })
    };
    eventually(|| gate.metrics().in_flight == 2).await;
    gate.close();
    assert!(matches!(task.await.unwrap(), Err(Error::Closed)));
    assert!(matches!(
        gate.acquire(CancellationToken::new()).await,
        Err(Error::Closed)
    ));
    let result = held.run(|| panic!("closed work must not start")).await;
    assert!(matches!(result, Err(Error::Closed | Error::Cancelled)));
    eventually(|| gate.metrics().in_flight == 0).await;
}

#[tokio::test]
async fn maintenance_has_reserved_fifo_slot_and_no_start_deadline() {
    let gate = gate(1, Duration::from_millis(40));
    let held = gate.acquire(CancellationToken::new()).await.unwrap();
    let waiter = {
        let gate = gate.clone();
        tokio::spawn(async move {
            match gate.acquire(CancellationToken::new()).await {
                Ok(admitted) => admitted.run(|| 1).await,
                Err(error) => Err(error),
            }
        })
    };
    eventually(|| gate.metrics().in_flight == 2).await;
    let maintenance = {
        let gate = gate.clone();
        tokio::spawn(async move { gate.acquire_maintenance().await.unwrap().run(|| 2).await })
    };
    eventually(|| gate.metrics().in_flight == 3).await;
    assert!(matches!(
        gate.acquire(CancellationToken::new()).await,
        Err(Error::Full)
    ));
    // The request expires; the reserved maintenance request must remain queued.
    assert!(matches!(waiter.await.unwrap(), Err(Error::Expired)));
    assert!(!maintenance.is_finished());
    drop(held);
    assert_eq!(maintenance.await.unwrap().unwrap(), 2);
    assert_eq!(gate.metrics().peak_running, 1);
}

#[tokio::test]
async fn maintenance_waiter_is_woken_by_shutdown() {
    let gate = gate(0, Duration::from_secs(1));
    let held = gate.acquire(CancellationToken::new()).await.unwrap();
    let maintenance = {
        let gate = gate.clone();
        tokio::spawn(async move { gate.acquire_maintenance().await })
    };
    eventually(|| gate.metrics().in_flight == 2).await;
    gate.close();
    assert!(matches!(maintenance.await.unwrap(), Err(Error::Closed)));
    drop(held);
}

#[test]
fn maintenance_waits_for_saturated_blocking_pool_without_expiring() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let gate = gate(0, Duration::from_millis(30));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let hog = tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        started_rx.await.unwrap();
        let task = {
            let gate = gate.clone();
            tokio::spawn(async move { gate.acquire_maintenance().await.unwrap().run(|| 42).await })
        };
        eventually(|| gate.metrics().blocking_tasks == 1).await;
        tokio::time::sleep(Duration::from_millis(70)).await;
        assert!(!task.is_finished());
        release_tx.send(()).unwrap();
        hog.await.unwrap();
        assert_eq!(task.await.unwrap().unwrap(), 42);
    });
}
