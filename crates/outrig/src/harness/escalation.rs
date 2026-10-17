//! Who answers a hosted request the session's policy holds for a person.
//!
//! A `[policy]` rule whose action is `escalate` -- or `evaluate`, while no evaluator is
//! configured -- holds the request in the session's gate and asks the session's escalation
//! handler, which answers allow or deny for that one request. Nothing is remembered: the next
//! request asks again. OutRig sets no deadline; the request waits until the handler answers, its
//! caller is interrupted, or the session closes, and a handler that wants a deadline denies when
//! it passes.
//!
//! The handler is trusted host code, and decides who may approve: being able to answer an agent's
//! ordinary questions is no authority over a hosted call. With no handler installed, a request
//! that would wait for one is denied, its reason saying no approver is installed.

use std::collections::BTreeMap;
use std::error::Error;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use tokio_util::sync::CancellationToken;

use super::event::{ExecId, Operation, PolicyVersion, RequestId};

/// What a handler's answer, or its failure, is.
pub type EscalationResult = Result<EscalationAnswer, Box<dyn Error + Send + Sync>>;

/// Answers each hosted request a session's policy escalates, installed with
/// [`SessionBuilder::escalation`](super::SessionBuilder::escalation).
///
/// Called once per request, on the session's runtime, as the request is held; several may be
/// pending at once. An `Err`, a panic, or the future being dropped denies the request, which is
/// never invoked. [`Escalation::withdrawn`] resolves when the request stops waiting -- its caller
/// was interrupted, or the session is closing -- after which an answer changes nothing, so an
/// interface can take the question back.
///
/// Implement it with `async fn escalate(&self, request: Escalation) -> EscalationResult`, or pass
/// any `Fn(Escalation) -> impl Future<Output = EscalationResult>`.
pub trait EscalationHandler: Send + Sync {
    fn escalate(&self, request: Escalation) -> impl Future<Output = EscalationResult> + Send;
}

impl<F, Fut> EscalationHandler for F
where
    F: Fn(Escalation) -> Fut + Send + Sync,
    Fut: Future<Output = EscalationResult> + Send,
{
    fn escalate(&self, request: Escalation) -> impl Future<Output = EscalationResult> + Send {
        self(request)
    }
}

/// A handler, its future boxed so the session can hold any one.
pub(crate) type Handler = Arc<dyn Erased>;

pub(crate) trait Erased: Send + Sync {
    fn escalate(
        self: Arc<Self>,
        request: Escalation,
    ) -> Pin<Box<dyn Future<Output = EscalationResult> + Send>>;
}

impl<H: EscalationHandler + 'static> Erased for H {
    fn escalate(
        self: Arc<Self>,
        request: Escalation,
    ) -> Pin<Box<dyn Future<Output = EscalationResult> + Send>> {
        Box::pin(async move { EscalationHandler::escalate(&*self, request).await })
    }
}

/// A hosted request held for an answer: everything an approver is shown, all of it built on the
/// host from what the binding decoded, without rendering an object.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Escalation {
    /// The request's id, which every event about it carries.
    pub request: RequestId,
    /// The request a callback made it inside, or `None`.
    pub parent: Option<RequestId>,
    pub agent: String,
    /// The execution the interpreter said made it, or `None`. It is the container's claim, not
    /// something the host observed.
    pub execid: Option<ExecId>,
    pub binding: String,
    pub operation: Operation,
    /// The member it names. A call of a callable read from a member is shown as a call of that
    /// member, with the call's own arguments.
    pub member: Option<String>,
    /// The qualified name of the host-side type of the object it is made on.
    pub target: Option<String>,
    /// A bounded preview of each positional argument, built from what crossed by value; an object
    /// of the host's is named by its type and a reference number.
    pub arguments: Vec<String>,
    /// A preview of each keyword argument, by name.
    pub keywords: BTreeMap<String, String>,
    /// Why it was escalated: the rule and its layer, or the default.
    pub reason: String,
    /// The rules in force when it was decided.
    pub policy: PolicyVersion,
    pub(crate) withdrawal: Withdrawal,
}

impl Escalation {
    /// Resolves once the request stops waiting for this answer, and says why. An answer given
    /// after it changes nothing, and is recorded as ignored.
    pub async fn withdrawn(&self) -> Withdrawn {
        self.withdrawal.token.cancelled().await;
        self.withdrawal.why()
    }

    /// Why the request stopped waiting, once it has.
    pub fn is_withdrawn(&self) -> Option<Withdrawn> {
        self.withdrawal
            .token
            .is_cancelled()
            .then(|| self.withdrawal.why())
    }
}

/// A handler's answer for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EscalationAnswer {
    /// Run it. The binding invokes it even if the session closes after this answer won.
    Allow,
    /// Refuse it, saying why: the caller's exception and the request's `refused` outcome carry
    /// the reason.
    Deny(String),
}

/// Why a request stopped waiting for its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Withdrawn {
    /// Its caller was interrupted, or cancelled.
    Interrupted,
    /// The session is closing.
    Closed,
    /// Its connection closed.
    Disconnected,
    /// The process holding the session's bindings ended.
    Exited,
}

/// The gate's side of [`Escalation::withdrawn`].
#[derive(Debug, Clone, Default)]
pub(crate) struct Withdrawal {
    token: CancellationToken,
    reason: Arc<OnceLock<Withdrawn>>,
}

impl Withdrawal {
    /// Tell the handler the request stopped waiting, for `why`. The first reason given stands.
    pub(crate) fn withdraw(&self, why: Withdrawn) {
        let _ = self.reason.set(why);
        self.token.cancel();
    }

    fn why(&self) -> Withdrawn {
        *self
            .reason
            .get()
            .expect("a withdrawal's reason is set before it is cancelled")
    }
}
