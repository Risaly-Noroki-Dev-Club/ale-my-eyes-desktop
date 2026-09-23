#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use ale_gui::AppWindow;
use slint::ComponentHandle;

fn with_ui_runtime<T>(runtime: &tokio::runtime::Runtime, run: impl FnOnce() -> T) -> T {
    // Slint owns this thread's event loop and polls its own futures. Wrapping it
    // in block_on would retain one Tokio cooperative budget for the entire UI
    // lifetime, eventually making otherwise-ready timers and joins stay Pending.
    let _context = runtime.enter();
    run()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if let Some(result) = ale_gui::diagnostics::dispatch_internal_mode() {
        return result.map_err(|error| std::io::Error::other(error).into());
    }
    if let Some(result) = ale_gui::dispatch_platform_worker() {
        return result.map_err(|error| std::io::Error::other(error).into());
    }
    #[cfg(target_os = "windows")]
    if std::env::var_os("SLINT_BACKEND").is_none() {
        std::env::set_var("SLINT_BACKEND", "winit-software");
    }
    let diagnostics = ale_gui::diagnostics::bootstrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create async runtime");
    let heartbeat = diagnostics.clone();
    runtime.spawn(async move {
        let mut timer = tokio::time::interval(std::time::Duration::from_millis(250));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            heartbeat.runtime_heartbeat();
            if heartbeat
                .status
                .shutdown
                .load(std::sync::atomic::Ordering::Acquire)
            {
                break;
            }
        }
    });
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).map(String::as_str) == Some("--modeld-supervisor-check") {
        let models_dir = args
            .get(2)
            .ok_or("--modeld-supervisor-check requires models and report paths")?;
        let report_path = args
            .get(3)
            .ok_or("--modeld-supervisor-check requires models and report paths")?;
        let result = runtime
            .block_on(ale_gui::run_modeld_supervisor_acceptance(
                models_dir.into(),
                report_path.into(),
            ))
            .map_err(std::io::Error::other);
        ale_gui::diagnostics::shutdown_requested();
        runtime.shutdown_timeout(std::time::Duration::from_secs(2));
        return result.map_err(Into::into);
    }
    let result: Result<(), Box<dyn std::error::Error>> = with_ui_runtime(&runtime, || {
        let app = {
            let _stage = ale_gui::diagnostics::stage("window_create");
            AppWindow::new()?
        };
        let supported = app
            .window()
            .set_rendering_notifier(|state, _| {
                let phase = match state {
                    slint::RenderingState::RenderingSetup => 1,
                    slint::RenderingState::BeforeRendering => 2,
                    slint::RenderingState::AfterRendering => 3,
                    slint::RenderingState::RenderingTeardown => 4,
                    _ => 0,
                };
                ale_gui::diagnostics::rendering_state(phase);
            })
            .is_ok();
        ale_gui::diagnostics::render_notifier_supported(supported);
        ale_core::diagnostics::record(
            "renderer_selected",
            &[
                (
                    "software",
                    u64::from(std::env::var("SLINT_BACKEND").is_ok_and(|s| s.contains("software"))),
                ),
                ("render_notifier_supported", u64::from(supported)),
            ],
        );
        let _acceptance = ale_gui::setup_app_for_launch(&app, &args)?;
        Ok(app.run()?)
    });
    runtime.block_on(ale_gui::finish_settings_writes());
    ale_gui::diagnostics::shutdown_requested();
    drop(diagnostics);
    // A native library may not return from a blocking call. Do not block closing
    // the GUI on Tokio's implicit, unlimited blocking-pool shutdown wait.
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

#[cfg(test)]
mod tests {
    use super::with_ui_runtime;
    use std::{
        panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc,
        },
        task::{Context, Poll, Wake, Waker},
        time::Duration,
    };

    const READY_MESSAGES: usize = 2048;

    fn before_deadline(run: impl FnOnce() + Send + 'static) {
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(catch_unwind(AssertUnwindSafe(run)));
        });
        // This deadline belongs to the test runner, outside the runtime whose
        // scheduling behavior is under test. Never join a stuck worker.
        if let Err(panic) = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("UI runtime context test exceeded its independent deadline")
        {
            resume_unwind(panic);
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    fn ready_messages() -> tokio::sync::mpsc::Receiver<usize> {
        let (sender, receiver) = tokio::sync::mpsc::channel(READY_MESSAGES);
        for message in 0..READY_MESSAGES {
            sender.try_send(message).unwrap();
        }
        receiver
    }

    #[derive(Default)]
    struct WakeCounter(AtomicUsize);

    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn ui_runtime_context_keeps_ready_futures_progressing() {
        before_deadline(|| {
            let runtime = runtime();
            with_ui_runtime(&runtime, || {
                let mut receiver = ready_messages();
                let mut context = Context::from_waker(Waker::noop());
                for message in 0..READY_MESSAGES {
                    assert_eq!(receiver.poll_recv(&mut context), Poll::Ready(Some(message)));
                }
            });
        });
    }

    #[test]
    fn block_on_around_an_external_event_loop_exhausts_its_budget() {
        before_deadline(|| {
            let runtime = runtime();
            let mut receiver = ready_messages();
            let wakes = Arc::new(WakeCounter::default());
            let waker = Waker::from(wakes.clone());
            let mut context = Context::from_waker(&waker);
            let completed = runtime.block_on(async {
                // This synchronous polling loop stands in for Slint's executor:
                // it cannot return Pending to the enclosing block_on future.
                let mut completed = 0;
                for message in 0..READY_MESSAGES {
                    match receiver.poll_recv(&mut context) {
                        Poll::Ready(Some(value)) => {
                            assert_eq!(value, message);
                            completed += 1;
                        }
                        Poll::Pending => break,
                        Poll::Ready(None) => panic!("queued messages disappeared"),
                    }
                }
                assert!(completed > 0 && completed < READY_MESSAGES);
                assert_eq!(receiver.len(), READY_MESSAGES - completed);
                let before = wakes.0.load(Ordering::Relaxed);
                for _ in 0..READY_MESSAGES {
                    assert_eq!(receiver.poll_recv(&mut context), Poll::Pending);
                }
                assert!(wakes.0.load(Ordering::Relaxed) > before);
                completed
            });
            eprintln!("enclosing block_on exhausted its budget after {completed} ready receives");
            // Returning from the enclosing poll releases its exhausted budget;
            // the still-ready queue is immediately readable again.
            with_ui_runtime(&runtime, || {
                for message in completed..READY_MESSAGES {
                    assert_eq!(receiver.poll_recv(&mut context), Poll::Ready(Some(message)));
                }
            });
        });
    }

    #[test]
    fn ui_runtime_context_supports_spawn_and_timers_without_block_on() {
        before_deadline(|| {
            let runtime = runtime();
            let (sender, receiver) = mpsc::channel();
            assert!(tokio::runtime::Handle::try_current().is_err());
            with_ui_runtime(&runtime, || {
                // Construct the timer on the UI thread to verify its context,
                // then let a real Tokio worker drive it while the UI waits.
                let timer = tokio::time::sleep(Duration::from_millis(1));
                tokio::spawn(async move {
                    timer.await;
                    sender.send(()).unwrap();
                });
                receiver.recv_timeout(Duration::from_secs(2)).unwrap();
            });
            assert!(tokio::runtime::Handle::try_current().is_err());
        });
    }
}
