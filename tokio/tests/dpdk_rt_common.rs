//! DPDK Scheduler compatibility tests
//!
//! These tests are extracted from rt_common.rs to run with serial isolation.
//! Each test runs in an isolated subprocess to avoid DPDK EAL conflicts.
//!
//! Original 38 tests from rt_common.rs dpdk_scheduler module.

#![cfg(all(feature = "full", target_os = "linux"))]
#![cfg(not(miri))]
#![allow(clippy::needless_range_loop)]
#![allow(dead_code)]

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;
use tokio::{task, time};
use tokio_test::{assert_err, assert_ok};

use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::{mpsc, Arc};
use std::task::{Context, Poll};
use std::thread;
use std::time::{Duration, Instant};

#[allow(dead_code)]
const NUM_WORKERS: usize = 1;

fn rt() -> Arc<Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_dpdk()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("DPDK runtime creation failed"),
    )
}

async fn yield_once() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn block_on_sync() {
    let rt = rt();

    let mut win = false;
    rt.block_on(async {
        win = true;
    });

    assert!(win);
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn block_on_async() {
    let rt = rt();

    let out = rt.block_on(async {
        let (tx, rx) = oneshot::channel();

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            tx.send("ZOMG").unwrap();
        });

        assert_ok!(rx.await)
    });

    assert_eq!(out, "ZOMG");
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_one_bg() {
    let rt = rt();

    let out = rt.block_on(async {
        let (tx, rx) = oneshot::channel();

        tokio::spawn(async move {
            tx.send("ZOMG").unwrap();
        });

        assert_ok!(rx.await)
    });

    assert_eq!(out, "ZOMG");
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_one_join() {
    let rt = rt();

    let out = rt.block_on(async {
        let (tx, rx) = oneshot::channel();

        let handle = tokio::spawn(async move {
            tx.send("ZOMG").unwrap();
            "DONE"
        });

        let msg = assert_ok!(rx.await);

        let out = assert_ok!(handle.await);
        assert_eq!(out, "DONE");

        msg
    });

    assert_eq!(out, "ZOMG");
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_two() {
    let rt = rt();

    let out = rt.block_on(async {
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        tokio::spawn(async move {
            assert_ok!(tx1.send("ZOMG"));
        });

        tokio::spawn(async move {
            let msg = assert_ok!(rx1.await);
            assert_ok!(tx2.send(msg));
        });

        assert_ok!(rx2.await)
    });

    assert_eq!(out, "ZOMG");
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_many_from_block_on() {
    use tokio::sync::mpsc;

    const ITER: usize = 200;

    let rt = rt();

    let out = rt.block_on(async {
        let (done_tx, mut done_rx) = mpsc::unbounded_channel();

        let mut txs = (0..ITER)
            .map(|i| {
                let (tx, rx) = oneshot::channel();
                let done_tx = done_tx.clone();

                tokio::spawn(async move {
                    let msg = assert_ok!(rx.await);
                    assert_eq!(i, msg);
                    assert_ok!(done_tx.send(msg));
                });

                tx
            })
            .collect::<Vec<_>>();

        drop(done_tx);

        thread::spawn(move || {
            for (i, tx) in txs.drain(..).enumerate() {
                assert_ok!(tx.send(i));
            }
        });

        let mut out = vec![];
        while let Some(i) = done_rx.recv().await {
            out.push(i);
        }

        out.sort_unstable();
        out
    });

    assert_eq!(ITER, out.len());

    for i in 0..ITER {
        assert_eq!(i, out[i]);
    }
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_many_from_task() {
    use tokio::sync::mpsc;

    const ITER: usize = 500;

    let rt = rt();

    let out = rt.block_on(async {
        tokio::spawn(async move {
            let (done_tx, mut done_rx) = mpsc::unbounded_channel();

            let mut txs = (0..ITER)
                .map(|i| {
                    let (tx, rx) = oneshot::channel();
                    let done_tx = done_tx.clone();

                    tokio::spawn(async move {
                        let msg = assert_ok!(rx.await);
                        assert_eq!(i, msg);
                        assert_ok!(done_tx.send(msg));
                    });

                    tx
                })
                .collect::<Vec<_>>();

            drop(done_tx);

            thread::spawn(move || {
                for (i, tx) in txs.drain(..).enumerate() {
                    assert_ok!(tx.send(i));
                }
            });

            let mut out = vec![];
            while let Some(i) = done_rx.recv().await {
                out.push(i);
            }

            out.sort_unstable();
            out
        })
        .await
        .unwrap()
    });

    assert_eq!(ITER, out.len());

    for i in 0..ITER {
        assert_eq!(i, out[i]);
    }
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_one_from_block_on_called_on_handle() {
    let rt = rt();
    let (tx, rx) = oneshot::channel();

    #[allow(clippy::async_yields_async)]
    let handle = rt.handle().block_on(async {
        tokio::spawn(async move {
            tx.send("ZOMG").unwrap();
            "DONE"
        })
    });

    let out = rt.block_on(async {
        let msg = assert_ok!(rx.await);

        let out = assert_ok!(handle.await);
        assert_eq!(out, "DONE");

        msg
    });

    assert_eq!(out, "ZOMG");
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_await_chain() {
    let rt = rt();

    let out = rt.block_on(async {
        assert_ok!(tokio::spawn(async { assert_ok!(tokio::spawn(async { "hello" }).await) }).await)
    });

    assert_eq!(out, "hello");
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn outstanding_tasks_dropped() {
    let rt = rt();

    let cnt = Arc::new(());

    rt.block_on(async {
        let cnt = cnt.clone();

        tokio::spawn(poll_fn(move |_| {
            assert_eq!(2, Arc::strong_count(&cnt));
            Poll::<()>::Pending
        }));
    });

    assert_eq!(2, Arc::strong_count(&cnt));

    drop(rt);

    assert_eq!(1, Arc::strong_count(&cnt));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore] // DPDK EAL can only be initialized once per process; this test creates 2 runtimes
fn nested_rt() {
    // Use catch_unwind instead of #[should_panic] because subprocess isolation
    // is incompatible with should_panic attribute
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rt1 = rt();
        let rt2 = rt();
        rt1.block_on(async { rt2.block_on(async { "hello" }) });
    }));
    assert!(result.is_err(), "Expected panic when nesting runtimes");
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore] // DPDK EAL can only be initialized once per process; this test creates 2 runtimes
fn create_rt_in_block_on() {
    let rt1 = rt();
    let rt2 = rt1.block_on(async { rt() });
    let out = rt2.block_on(async { "ZOMG" });

    assert_eq!(out, "ZOMG");
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn complete_block_on_under_load() {
    let rt = rt();

    rt.block_on(async {
        let (tx, rx) = oneshot::channel();

        // Spin hard
        tokio::spawn(async {
            loop {
                yield_once().await;
            }
        });

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            assert_ok!(tx.send(()));
        });

        assert_ok!(rx.await);
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn complete_task_under_load() {
    let rt = rt();

    rt.block_on(async {
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();

        // Spin hard
        tokio::spawn(async {
            loop {
                yield_once().await;
            }
        });

        thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            assert_ok!(tx1.send(()));
        });

        tokio::spawn(async move {
            assert_ok!(rx1.await);
            assert_ok!(tx2.send(()));
        });

        assert_ok!(rx2.await);
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_from_other_thread_idle() {
    let rt = rt();
    let handle = rt.clone();

    let (tx, rx) = oneshot::channel();

    thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));

        handle.spawn(async move {
            assert_ok!(tx.send(()));
        });
    });

    rt.block_on(async move {
        assert_ok!(rx.await);
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_from_other_thread_under_load() {
    let rt = rt();
    let handle = rt.clone();

    let (tx, rx) = oneshot::channel();

    thread::spawn(move || {
        handle.spawn(async move {
            assert_ok!(tx.send(()));
        });
    });

    rt.block_on(async move {
        // Spin hard
        tokio::spawn(async {
            loop {
                yield_once().await;
            }
        });

        assert_ok!(rx.await);
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn sleep_at_root() {
    let rt = rt();

    let now = Instant::now();
    let dur = Duration::from_millis(50);

    rt.block_on(async move {
        time::sleep(dur).await;
    });

    assert!(now.elapsed() >= dur);
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn sleep_in_spawn() {
    let rt = rt();

    let now = Instant::now();
    let dur = Duration::from_millis(50);

    rt.block_on(async move {
        let (tx, rx) = oneshot::channel();

        tokio::spawn(async move {
            time::sleep(dur).await;
            assert_ok!(tx.send(()));
        });

        assert_ok!(rx.await);
    });

    assert!(now.elapsed() >= dur);
}

#[cfg_attr(miri, ignore)] // No `socket` in miri.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn block_on_socket() {
    let rt = rt();

    rt.block_on(async move {
        let (tx, rx) = oneshot::channel();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let _ = listener.accept().await;
            tx.send(()).unwrap();
        });

        TcpStream::connect(&addr).await.unwrap();
        rx.await.unwrap();
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_from_blocking() {
    let rt = rt();

    let out = rt.block_on(async move {
        let inner = assert_ok!(
            tokio::task::spawn_blocking(|| { tokio::spawn(async move { "hello" }) }).await
        );

        assert_ok!(inner.await)
    });

    assert_eq!(out, "hello")
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn spawn_blocking_from_blocking() {
    let rt = rt();

    let out = rt.block_on(async move {
        let inner = assert_ok!(
            tokio::task::spawn_blocking(|| { tokio::task::spawn_blocking(|| "hello") }).await
        );

        assert_ok!(inner.await)
    });

    assert_eq!(out, "hello")
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn sleep_from_blocking() {
    let rt = rt();

    rt.block_on(async move {
        assert_ok!(
            tokio::task::spawn_blocking(|| {
                let now = std::time::Instant::now();
                let dur = Duration::from_millis(1);

                // use the futures' block_on fn to make sure we aren't setting
                // any Tokio context
                futures::executor::block_on(async {
                    tokio::time::sleep(dur).await;
                });

                assert!(now.elapsed() >= dur);
            })
            .await
        );
    });
}

#[cfg_attr(miri, ignore)] // No `socket` in miri.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn socket_from_blocking() {
    let rt = rt();

    rt.block_on(async move {
        let listener = assert_ok!(TcpListener::bind("127.0.0.1:0").await);
        let addr = assert_ok!(listener.local_addr());

        let peer = tokio::task::spawn_blocking(move || {
            // use the futures' block_on fn to make sure we aren't setting
            // any Tokio context
            futures::executor::block_on(async {
                assert_ok!(TcpStream::connect(addr).await);
            });
        });

        // Wait for the client to connect
        let _ = assert_ok!(listener.accept().await);

        assert_ok!(peer.await);
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
// IOCP requires setting the "max thread" concurrency value. The sane,
// default, is to set this to the number of cores. Threads that poll I/O
// become associated with the IOCP handle. Once those threads sleep for any
// reason (mutex), they yield their ownership.
//
// This test hits an edge case on windows where more threads than cores are
// created, none of those threads ever yield due to being at capacity, so
// IOCP gets "starved".
//
// For now, this is a very edge case that is probably not a real production
// concern. There also isn't a great/obvious solution to take. For now, the
// test is disabled.
#[cfg(not(windows))]
#[cfg_attr(miri, ignore)] // No `socket` in miri.
fn io_driver_called_when_under_load() {
    let rt = rt();

    // Create a lot of constant load. The scheduler will always be busy.
    for _ in 0..100 {
        rt.spawn(async {
            loop {
                // Don't use Tokio's `yield_now()` to avoid special defer
                // logic.
                std::future::poll_fn::<(), _>(|cx| {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                })
                .await;
            }
        });
    }

    // Do some I/O work
    rt.block_on(async {
        let listener = assert_ok!(TcpListener::bind("127.0.0.1:0").await);
        let addr = assert_ok!(listener.local_addr());

        let srv = tokio::spawn(async move {
            let (mut stream, _) = assert_ok!(listener.accept().await);
            assert_ok!(stream.write_all(b"hello world").await);
        });

        let cli = tokio::spawn(async move {
            let mut stream = assert_ok!(TcpStream::connect(addr).await);
            let mut dst = vec![0; 11];

            assert_ok!(stream.read_exact(&mut dst).await);
            assert_eq!(dst, b"hello world");
        });

        assert_ok!(srv.await);
        assert_ok!(cli.await);
    });
}

/// Tests that yielded tasks are not scheduled until **after** resource
/// drivers are polled.
///
/// The OS does not guarantee when I/O events are delivered, so there may be
/// more yields than anticipated. This makes the test slightly flaky. To
/// help avoid flakiness, we run the test 10 times and only fail it after
/// 10 failures in a row.
///
/// Note that if the test fails by panicking rather than by returning false,
/// then we fail it immediately. That kind of failure should not happen
/// spuriously.
#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore] // DPDK EAL can only be initialized once per process; retry loop creates multiple runtimes
#[cfg_attr(miri, ignore)] // No `socket` in miri.
fn yield_defers_until_park() {
    for _ in 0..10 {
        if yield_defers_until_park_inner(false) {
            // test passed
            return;
        }

        // Wait a bit and run the test again.
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    panic!("yield_defers_until_park is failing consistently");
}

/// Same as above, but with cooperative scheduling.
#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore] // DPDK EAL can only be initialized once per process; retry loop creates multiple runtimes
#[cfg_attr(miri, ignore)] // No `socket` in miri.
fn coop_yield_defers_until_park() {
    for _ in 0..10 {
        if yield_defers_until_park_inner(true) {
            // test passed
            return;
        }

        // Wait a bit and run the test again.
        std::thread::sleep(std::time::Duration::from_secs(2));
    }

    panic!("yield_defers_until_park is failing consistently");
}

/// Implementation of `yield_defers_until_park` test. Returns `true` if the
/// test passed.
fn yield_defers_until_park_inner(use_coop: bool) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
    use std::sync::Barrier;

    const BUDGET: usize = 128;

    let rt = rt();

    let flag = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(NUM_WORKERS));

    rt.block_on(async {
        // Make sure other workers cannot steal tasks
        #[allow(clippy::reversed_empty_ranges)]
        for _ in 0..(NUM_WORKERS - 1) {
            let flag = flag.clone();
            let barrier = barrier.clone();

            tokio::spawn(async move {
                barrier.wait();

                while !flag.load(SeqCst) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            });
        }

        barrier.wait();

        let (fail_test, fail_test_recv) = oneshot::channel::<()>();
        let flag_clone = flag.clone();
        let jh = tokio::spawn(async move {
            // Create a TCP listener
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            tokio::join!(
                async {
                    // Done in a blocking manner intentionally.
                    let _socket = std::net::TcpStream::connect(addr).unwrap();

                    // Yield until connected
                    let mut cnt = 0;
                    while !flag_clone.load(SeqCst) {
                        if use_coop {
                            // Consume a good chunk of budget, which should
                            // force at least one yield.
                            for _ in 0..BUDGET {
                                tokio::task::consume_budget().await;
                            }
                        } else {
                            tokio::task::yield_now().await;
                        }
                        cnt += 1;

                        if cnt >= 10 {
                            // yielded too many times; report failure and
                            // sleep forever so that the `fail_test` branch
                            // of the `select!` below triggers.
                            let _ = fail_test.send(());
                            futures::future::pending::<()>().await;
                            break;
                        }
                    }
                },
                async {
                    let _ = listener.accept().await.unwrap();
                    flag_clone.store(true, SeqCst);
                }
            );
        });

        // Wait until the spawned task completes or fails. If no message is
        // sent on `fail_test`, then the test succeeds. Otherwise, it fails.
        let success = fail_test_recv.await.is_err();

        if success {
            // Setting flag to true ensures that the tasks we spawned at
            // the beginning of the test will exit.
            // If we don't do this, the test will hang since the runtime waits
            // for all spawned tasks to finish when dropping.
            flag.store(true, SeqCst);
            // Check for panics in spawned task.
            jh.abort();
            jh.await.unwrap();
        }

        success
    })
}

#[cfg_attr(miri, ignore)] // No `socket` in miri.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn client_server_block_on() {
    let rt = rt();
    let (tx, rx) = mpsc::channel();

    rt.block_on(async move { client_server(tx).await });

    assert_ok!(rx.try_recv());
    assert_err!(rx.try_recv());
}

#[cfg_attr(
    target_os = "wasi",
    ignore = "Wasi does not support threads or panic recovery"
)]
#[cfg(panic = "unwind")]
#[serial_isolation_test::serial_isolation_test]
#[test]
fn panic_in_task() {
    let rt = rt();
    let (tx, rx) = oneshot::channel();

    struct Boom(Option<oneshot::Sender<()>>);

    impl Future for Boom {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            panic!();
        }
    }

    impl Drop for Boom {
        fn drop(&mut self) {
            assert!(std::thread::panicking());
            self.0.take().unwrap().send(()).unwrap();
        }
    }

    rt.spawn(Boom(Some(tx)));
    assert_ok!(rt.block_on(rx));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[cfg_attr(target_os = "wasi", ignore = "Wasi does not support panic recovery")]
fn panic_in_block_on() {
    // Use catch_unwind instead of #[should_panic] because subprocess isolation
    // is incompatible with should_panic attribute
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let rt = rt();
        rt.block_on(async { panic!("intentional panic for test") });
    }));
    assert!(result.is_err(), "Expected panic in block_on to propagate");
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn enter_and_spawn() {
    let rt = rt();
    let handle = {
        let _enter = rt.enter();
        tokio::spawn(async {})
    };

    assert_ok!(rt.block_on(handle));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn eagerly_drops_futures_on_shutdown() {
    use std::sync::mpsc;

    struct Never {
        drop_tx: mpsc::Sender<()>,
    }

    impl Future for Never {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            Poll::Pending
        }
    }

    impl Drop for Never {
        fn drop(&mut self) {
            self.drop_tx.send(()).unwrap();
        }
    }

    let rt = rt();

    let (drop_tx, drop_rx) = mpsc::channel();
    let (run_tx, run_rx) = oneshot::channel();

    rt.block_on(async move {
        tokio::spawn(async move {
            assert_ok!(run_tx.send(()));

            Never { drop_tx }.await
        });

        assert_ok!(run_rx.await);
    });

    drop(rt);

    assert_ok!(drop_rx.recv());
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn wake_while_rt_is_dropping() {
    use tokio::sync::Barrier;

    struct OnDrop<F: FnMut()>(F);

    impl<F: FnMut()> Drop for OnDrop<F> {
        fn drop(&mut self) {
            (self.0)()
        }
    }

    let (tx1, rx1) = oneshot::channel();
    let (tx2, rx2) = oneshot::channel();

    let barrier = Arc::new(Barrier::new(3));
    let barrier1 = barrier.clone();
    let barrier2 = barrier.clone();

    let rt = rt();

    rt.spawn(async move {
        let mut tx2 = Some(tx2);
        let _d = OnDrop(move || {
            let _ = tx2.take().unwrap().send(());
        });

        // Ensure a waker gets stored in oneshot 1.
        let _ = tokio::join!(rx1, barrier1.wait());
    });

    rt.spawn(async move {
        let mut tx1 = Some(tx1);
        let _d = OnDrop(move || {
            let _ = tx1.take().unwrap().send(());
        });

        // Ensure a waker gets stored in oneshot 2.
        let _ = tokio::join!(rx2, barrier2.wait());
    });

    // Wait until every oneshot channel has been polled.
    rt.block_on(barrier.wait());

    // Drop the rt. Regardless of which task is dropped first, its destructor will wake the
    // other task.
    drop(rt);
}

#[cfg_attr(miri, ignore)] // No `socket` in miri.
#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore] // DPDK EAL can only be initialized once per process; loop creates multiple runtimes
fn io_notify_while_shutting_down() {
    use std::sync::Arc;
    use tokio::net::UdpSocket;

    for _ in 1..10 {
        let runtime = rt();

        runtime.block_on(async {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let addr = socket.local_addr().unwrap();
            let send_half = Arc::new(socket);
            let recv_half = send_half.clone();

            tokio::spawn(async move {
                let mut buf = [0];
                loop {
                    recv_half.recv_from(&mut buf).await.unwrap();
                    std::thread::sleep(Duration::from_millis(2));
                }
            });

            tokio::spawn(async move {
                let buf = [0];
                loop {
                    send_half.send_to(&buf, &addr).await.unwrap();
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            });

            tokio::time::sleep(Duration::from_millis(5)).await;
        });
    }
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn shutdown_timeout() {
    let (tx, rx) = oneshot::channel();
    let runtime = rt();

    runtime.block_on(async move {
        task::spawn_blocking(move || {
            tx.send(()).unwrap();
            thread::sleep(Duration::from_secs(10_000));
        });

        rx.await.unwrap();
    });

    Arc::try_unwrap(runtime)
        .unwrap()
        .shutdown_timeout(Duration::from_millis(100));
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn shutdown_timeout_0() {
    let runtime = rt();

    runtime.block_on(async move {
        task::spawn_blocking(move || {
            thread::sleep(Duration::from_secs(10_000));
        });
    });

    let now = Instant::now();
    Arc::try_unwrap(runtime)
        .unwrap()
        .shutdown_timeout(Duration::from_nanos(0));
    assert!(now.elapsed().as_secs() < 1);
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn shutdown_wakeup_time() {
    let runtime = rt();

    runtime.block_on(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    });

    Arc::try_unwrap(runtime)
        .unwrap()
        .shutdown_timeout(Duration::from_secs(10_000));
}

// This test is currently ignored on Windows because of a
// rust-lang issue in thread local storage destructors.
// See https://github.com/rust-lang/rust/issues/74875
#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore] // DPDK EAL can only be initialized once per process; this test creates 2 runtimes
#[cfg(not(windows))]
#[cfg_attr(target_os = "wasi", ignore = "Wasi does not support threads")]
fn runtime_in_thread_local() {
    use std::cell::RefCell;
    use std::thread;

    thread_local!(
        static R: RefCell<Option<Runtime>> = const { RefCell::new(None) };
    );

    thread::spawn(|| {
        R.with(|cell| {
            let rt = rt();
            let rt = Arc::try_unwrap(rt).unwrap();
            *cell.borrow_mut() = Some(rt);
        });

        let _rt = rt();
    })
    .join()
    .unwrap();
}

async fn client_server(tx: mpsc::Sender<()>) {
    let server = assert_ok!(TcpListener::bind("127.0.0.1:0").await);

    // Get the assigned address
    let addr = assert_ok!(server.local_addr());

    // Spawn the server
    tokio::spawn(async move {
        // Accept a socket
        let (mut socket, _) = server.accept().await.unwrap();

        // Write some data
        socket.write_all(b"hello").await.unwrap();
    });

    let mut client = TcpStream::connect(&addr).await.unwrap();

    let mut buf = vec![];
    client.read_to_end(&mut buf).await.unwrap();

    assert_eq!(buf, b"hello");
    tx.send(()).unwrap();
}

#[cfg(not(target_os = "wasi"))] // Wasi does not support bind
#[cfg_attr(miri, ignore)] // No `socket` in miri.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn local_set_block_on_socket() {
    let rt = rt();
    let local = task::LocalSet::new();

    local.block_on(&rt, async move {
        let (tx, rx) = oneshot::channel();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        task::spawn_local(async move {
            let _ = listener.accept().await;
            tx.send(()).unwrap();
        });

        TcpStream::connect(&addr).await.unwrap();
        rx.await.unwrap();
    });
}

#[cfg(not(target_os = "wasi"))] // Wasi does not support bind
#[cfg_attr(miri, ignore)] // No `socket` in miri.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn local_set_client_server_block_on() {
    let rt = rt();
    let (tx, rx) = mpsc::channel();

    let local = task::LocalSet::new();

    local.block_on(&rt, async move { client_server_local(tx).await });

    assert_ok!(rx.try_recv());
    assert_err!(rx.try_recv());
}

#[cfg(not(target_os = "wasi"))] // Wasi does not support bind
async fn client_server_local(tx: mpsc::Sender<()>) {
    let server = assert_ok!(TcpListener::bind("127.0.0.1:0").await);

    // Get the assigned address
    let addr = assert_ok!(server.local_addr());

    // Spawn the server
    task::spawn_local(async move {
        // Accept a socket
        let (mut socket, _) = server.accept().await.unwrap();

        // Write some data
        socket.write_all(b"hello").await.unwrap();
    });

    let mut client = TcpStream::connect(&addr).await.unwrap();

    let mut buf = vec![];
    client.read_to_end(&mut buf).await.unwrap();

    assert_eq!(buf, b"hello");
    tx.send(()).unwrap();
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn coop() {
    use std::task::Poll::Ready;
    use tokio::sync::mpsc;

    let rt = rt();

    rt.block_on(async {
        let (send, mut recv) = mpsc::unbounded_channel();

        // Send a bunch of messages.
        for _ in 0..1_000 {
            send.send(()).unwrap();
        }

        poll_fn(|cx| {
            // At least one response should return pending.
            for _ in 0..1_000 {
                if recv.poll_recv(cx).is_pending() {
                    return Ready(());
                }
            }

            panic!("did not yield");
        })
        .await;
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
fn coop_unconstrained() {
    use std::task::Poll::Ready;
    use tokio::sync::mpsc;

    let rt = rt();

    rt.block_on(async {
        let (send, mut recv) = mpsc::unbounded_channel();

        // Send a bunch of messages.
        for _ in 0..1_000 {
            send.send(()).unwrap();
        }

        tokio::task::unconstrained(poll_fn(|cx| {
            // All the responses should be ready.
            for _ in 0..1_000 {
                assert_eq!(recv.poll_recv(cx), Poll::Ready(Some(())));
            }

            Ready(())
        }))
        .await;
    });
}

#[cfg(tokio_unstable)]
#[serial_isolation_test::serial_isolation_test]
#[test]
fn coop_consume_budget() {
    let rt = rt();

    rt.block_on(async {
        poll_fn(|cx| {
            let counter = Arc::new(std::sync::Mutex::new(0));
            let counter_clone = Arc::clone(&counter);
            let mut worker = Box::pin(async move {
                // Consume the budget until a yield happens
                for _ in 0..1000 {
                    *counter.lock().unwrap() += 1;
                    task::consume_budget().await
                }
            });
            // Assert that the worker was yielded and it didn't manage
            // to finish the whole work (assuming the total budget of 128)
            assert!(Pin::new(&mut worker).poll(cx).is_pending());
            assert!(*counter_clone.lock().unwrap() < 1000);
            std::task::Poll::Ready(())
        })
        .await;
    });
}

// Tests that the "next task" scheduler optimization is not able to starve
// other tasks.
#[serial_isolation_test::serial_isolation_test]
#[test]
fn ping_pong_saturation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::sync::mpsc;

    const NUM: usize = 100;

    let rt = rt();

    let running = Arc::new(AtomicBool::new(true));

    rt.block_on(async {
        let (spawned_tx, mut spawned_rx) = mpsc::unbounded_channel();

        let mut tasks = vec![];
        // Spawn a bunch of tasks that ping-pong between each other to
        // saturate the runtime.
        for _ in 0..NUM {
            let (tx1, mut rx1) = mpsc::unbounded_channel();
            let (tx2, mut rx2) = mpsc::unbounded_channel();
            let spawned_tx = spawned_tx.clone();
            let running = running.clone();
            tasks.push(task::spawn(async move {
                spawned_tx.send(()).unwrap();

                while running.load(Ordering::Relaxed) {
                    tx1.send(()).unwrap();
                    rx2.recv().await.unwrap();
                }

                // Close the channel and wait for the other task to exit.
                drop(tx1);
                assert!(rx2.recv().await.is_none());
            }));

            tasks.push(task::spawn(async move {
                while rx1.recv().await.is_some() {
                    tx2.send(()).unwrap();
                }
            }));
        }

        for _ in 0..NUM {
            spawned_rx.recv().await.unwrap();
        }

        // spawn another task and wait for it to complete
        let handle = task::spawn(async {
            for _ in 0..5 {
                // Yielding forces it back into the local queue.
                task::yield_now().await;
            }
        });
        handle.await.unwrap();
        running.store(false, Ordering::Relaxed);
        for t in tasks {
            t.await.unwrap();
        }
    });
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[ignore] // DPDK EAL can only be initialized once per process; loop creates multiple runtimes
fn shutdown_concurrent_spawn() {
    const NUM_TASKS: usize = 10_000;
    for _ in 0..5 {
        let (tx, rx) = std::sync::mpsc::channel();
        let rt = rt();

        let mut txs = vec![];

        for _ in 0..NUM_TASKS {
            let (tx, rx) = tokio::sync::oneshot::channel();
            txs.push(tx);
            rt.spawn(async move {
                rx.await.unwrap();
            });
        }

        // Prime the tasks
        rt.block_on(async { tokio::task::yield_now().await });

        let th = std::thread::spawn(move || {
            tx.send(()).unwrap();
            for tx in txs.drain(..) {
                let _ = tx.send(());
            }
        });

        rx.recv().unwrap();
        drop(rt);

        th.join().unwrap();
    }
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[cfg_attr(target_family = "wasm", ignore)]
fn wake_by_ref_from_thread_local() {
    wake_from_thread_local(true);
}

#[serial_isolation_test::serial_isolation_test]
#[test]
#[cfg_attr(target_family = "wasm", ignore)]
fn wake_by_val_from_thread_local() {
    wake_from_thread_local(false);
}

fn wake_from_thread_local(by_ref: bool) {
    use std::cell::RefCell;
    use std::sync::mpsc::{channel, Sender};
    use std::task::Waker;

    struct TLData {
        by_ref: bool,
        waker: Option<Waker>,
        done: Sender<bool>,
    }

    impl Drop for TLData {
        fn drop(&mut self) {
            if self.by_ref {
                self.waker.take().unwrap().wake_by_ref();
            } else {
                self.waker.take().unwrap().wake();
            }
            let _ = self.done.send(true);
        }
    }

    std::thread_local! {
        static TL_DATA: RefCell<Option<TLData>> = const { RefCell::new(None) };
    };

    let (send, recv) = channel();

    std::thread::spawn(move || {
        let rt = rt();
        rt.block_on(rt.spawn(poll_fn(move |cx| {
            let waker = cx.waker().clone();
            let send = send.clone();
            TL_DATA.with(|tl| {
                tl.replace(Some(TLData {
                    by_ref,
                    waker: Some(waker),
                    done: send,
                }));
            });
            Poll::Ready(())
        })))
        .unwrap();
    })
    .join()
    .unwrap();

    assert!(recv.recv().unwrap());
}
