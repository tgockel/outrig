//! The signal listeners `outrig run` and `outrig mcp` install before session
//! setup, driven by real signals.
//!
//! A test binary of its own because the signals are raised at this process.
//! Every case installs its listeners before it raises, so nothing ever reaches
//! a default disposition, but a listener anywhere else in the process would
//! hear them too. One test, with the cases in sequence, for the same reason.

use std::future::pending;
use std::time::Duration;

use nix::sys::signal::{Signal, raise};
use outrig_cli::cli::signals::{EndSignal, Signals, unless_signaled};
use outrig_cli::error::CliError;
use tokio::time::{sleep, timeout};

/// Enough for the runtime's signal driver to have handled a raised signal.
/// `raise` runs the handler before it returns, and the driver broadcasts on
/// the runtime's next park, which any sleep is.
const SETTLE: Duration = Duration::from_millis(5);

/// Ceiling for a wait that should end on a signal already raised.
const CEILING: Duration = Duration::from_secs(10);

#[tokio::test]
async fn signals_end_a_session_whenever_they_land() {
    a_signal_nobody_was_waiting_for_wins_the_next_race().await;
    a_signal_during_a_race_ends_it().await;
    the_repl_wait_passes_over_sigint().await;
    cleanup_yields_to_sigint_and_sigterm_but_not_sighup().await;
    cleanup_ignores_signals_from_before_it_began().await;
    exit_codes_are_128_plus_the_signal_number();
}

/// The property every unraced phase leans on: a signal that lands between
/// races is deferred to the next one, not lost. And the signal wins the tie
/// against a future that is ready at once.
async fn a_signal_nobody_was_waiting_for_wins_the_next_race() {
    let mut signals = Signals::install().expect("install listeners");
    raise(Signal::SIGHUP).expect("raise SIGHUP");
    sleep(SETTLE).await;

    let result = signals.race(async { Ok::<_, CliError>(()) }).await;
    assert!(
        matches!(result, Err(CliError::Interrupted(EndSignal::Hangup))),
        "a pending SIGHUP should win the race: {result:?}"
    );
}

async fn a_signal_during_a_race_ends_it() {
    let mut signals = Signals::install().expect("install listeners");
    let waiting = async {
        raise(Signal::SIGTERM).expect("raise SIGTERM");
        pending::<Result<(), CliError>>().await
    };

    let result = timeout(CEILING, signals.race(waiting))
        .await
        .expect("the race should end on the signal");
    assert!(
        matches!(result, Err(CliError::Interrupted(EndSignal::Terminate))),
        "SIGTERM should end the race: {result:?}"
    );
}

async fn the_repl_wait_passes_over_sigint() {
    let mut signals = Signals::install().expect("install listeners");
    raise(Signal::SIGINT).expect("raise SIGINT");
    // Settled first, so a wait that heard SIGINT would find it ready alone and
    // fail every time, not only when it happened to pick it over SIGTERM.
    sleep(SETTLE).await;
    raise(Signal::SIGTERM).expect("raise SIGTERM");

    let sig = timeout(CEILING, signals.termination())
        .await
        .expect("SIGTERM should end the wait");
    assert_eq!(sig, EndSignal::Terminate);
}

async fn cleanup_yields_to_sigint_and_sigterm_but_not_sighup() {
    for (raised, expected) in [
        (Signal::SIGINT, EndSignal::Interrupt),
        (Signal::SIGTERM, EndSignal::Terminate),
    ] {
        let cleanup = async move {
            raise(raised).expect("raise");
            pending::<()>().await;
        };
        let cut = timeout(CEILING, unless_signaled(cleanup))
            .await
            .expect("the signal should cut cleanup short");
        assert_eq!(cut, Some(expected));
    }

    let cleanup = async {
        raise(Signal::SIGHUP).expect("raise SIGHUP");
        sleep(SETTLE).await;
    };
    assert_eq!(
        unless_signaled(cleanup).await,
        None,
        "SIGHUP should let cleanup finish"
    );
}

/// The signal that ended the session must not also end its cleanup.
async fn cleanup_ignores_signals_from_before_it_began() {
    let _session = Signals::install().expect("install listeners");
    raise(Signal::SIGTERM).expect("raise SIGTERM");
    sleep(SETTLE).await;

    assert_eq!(unless_signaled(sleep(SETTLE)).await, None);
}

fn exit_codes_are_128_plus_the_signal_number() {
    assert_eq!(EndSignal::Hangup.exit_code(), 129);
    assert_eq!(EndSignal::Interrupt.exit_code(), 130);
    assert_eq!(EndSignal::Terminate.exit_code(), 143);
    assert_eq!(CliError::Interrupted(EndSignal::Terminate).exit_code(), 143);
}
