//! Request liveness for the primary whose owner calls `Session::round`.
//!
//! Only a successfully ended round advances the baseline. Failed or dropped
//! rounds must not spend a warning the model may not have read. Ends awaiting
//! acknowledgment survive a dropped future and are retried before another round.

use super::AgentError;
use super::channel::Announcer;
use super::children::{Registry, Summary};
use super::{orientation, runner::SETTLE_WAIT};
use crate::harness::event::ChildKind;
use crate::python::host::PRIMARY;

pub(super) struct Requests {
    registry: Registry,
    baseline: Summary,
    warned: bool,
    ending: Summary,
}

pub(super) struct Opening {
    live: Summary,
    warning: bool,
    pub(super) lead: String,
    pub(super) detail: String,
}

impl Requests {
    pub(super) fn new(registry: Registry) -> Self {
        Self {
            registry,
            baseline: Summary::default(),
            warned: false,
            ending: Summary::default(),
        }
    }

    pub(super) async fn opening(
        &mut self,
        announcer: &Announcer,
    ) -> Result<Option<Opening>, AgentError> {
        let Some(children) = self.registry.upgrade() else {
            return Ok(None);
        };
        loop {
            let live = children.summary(PRIMARY, ChildKind::Request);
            // Once decided, an end is retried even if this call is dropped.
            let held = live.held();
            self.ending.unread.retain(|id| held.contains(id));
            self.ending.taken.retain(|id| held.contains(id));
            if !self.ending.is_empty() {
                for request in self.ending.held() {
                    let without = if self.ending.taken.contains(&request) {
                        "answering"
                    } else {
                        "reading"
                    };
                    children.settle(
                        PRIMARY,
                        request,
                        "cancelled",
                        &format!(
                            "request {request} was cancelled: the primary ended its rounds \
                                  without {without} it"
                        ),
                        None,
                    );
                }
                if children.closed() {
                    return Ok(None);
                }
                let _ = tokio::time::timeout(SETTLE_WAIT, children.primary_ended()).await;
                continue;
            }
            // Read announcements after any ends have drained: a request or message
            // arriving during that wait must be counted as told by this opening.
            let news = announcer.opening().await?;
            let live = children.summary(PRIMARY, ChildKind::Request);
            if live.is_empty() {
                return Ok(news.map(|lead| Opening {
                    live,
                    warning: false,
                    lead,
                    detail: String::new(),
                }));
            }
            let warning = live == self.baseline && news.is_none();
            if warning && self.warned {
                self.ending = live;
                continue;
            }
            // Include every unread request, not just newly announced ones.
            let unread = news.unwrap_or_else(|| {
                super::channel::announcement(&children.counts(PRIMARY)).unwrap_or_default()
            });
            let outstanding = orientation::requests(&unread, &children.taken(PRIMARY));
            let (lead, detail) = if warning {
                orientation::requests_warning(&outstanding, &children.unread_on(PRIMARY))
            } else {
                (outstanding, String::new())
            };
            return Ok(Some(Opening {
                live,
                warning,
                lead,
                detail,
            }));
        }
    }

    pub(super) fn ended(&mut self, opening: Opening) {
        self.baseline = opening.live;
        self.warned = opening.warning;
    }
}
