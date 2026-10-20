//! A session starts on whatever task its owner chooses: the futures that start
//! one are `Send`, so `tokio::spawn` takes them, with a progress callback or
//! without, with a container given or the default one, and with a primary
//! agent or none.
//!
//! Nothing here runs: what is checked is that each future compiles as a
//! spawnable one. A borrow of something not `Sync` held across an await
//! inside `start` is what this catches.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;

use outrig::LaunchSpec;
use outrig::config::Config;
use outrig::harness::{DEFAULT_DRAIN, Session, SessionBuilder};

fn spawnable<F: Future + Send + 'static>(_: F) {}

/// A session stops on whatever task its owner chooses too: the engine shuts down the sessions a
/// host program leaves open on tasks of their own. Never called; that it compiles is the check.
#[expect(dead_code, reason = "compiled, not run")]
fn a_shutdown_can_be_spawned(session: Session) {
    spawnable(session.shutdown(DEFAULT_DRAIN));
}

#[test]
fn every_way_to_start_a_session_can_be_spawned() {
    spawnable(SessionBuilder::new(Config::default(), None, None).start());
    spawnable(
        SessionBuilder::new(Config::default(), None, None)
            .progress(|_| {})
            .start(),
    );
    spawnable(
        SessionBuilder::new(Config::default(), None, None)
            .progress(|_| {})
            .container(LaunchSpec::from_image(
                "alpine",
                BTreeMap::new(),
                PathBuf::from("logs"),
            ))
            .start(),
    );
    spawnable(
        SessionBuilder::new(Config::default(), None, None)
            .no_primary()
            .start(),
    );
    spawnable(Session::start(Config::default(), None, None));
}
