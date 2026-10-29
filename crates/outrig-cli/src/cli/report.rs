//! What the terminal says of a session's shutdown report: `outrig run-new`'s at its end, and
//! `outrig run-new <script.py>`'s for each session a program left for the engine to stop.

use outrig::harness::{
    self, ExecutionStatus, LeftOpen, SessionEnd, ShutdownReport, Stopped, Verdict,
};

/// What the terminal says of a report.
#[derive(Debug)]
pub(crate) struct Summary {
    pub(crate) verdict: Verdict,
    /// What closed the session, when it was not its owner: the interpreter
    /// exiting.
    pub(crate) closed: Option<String>,
    /// Why it is not proven stopped, when it is not.
    pub(crate) not_stopped: Option<String>,
    pub(crate) executions: Vec<(u64, ExecutionStatus)>,
    /// Each call into a binding whose outcome came after the close, as a line.
    pub(crate) hosted: Vec<String>,
    pub(crate) last_sequence: u64,
    /// What the terminal's own subscription missed.
    pub(crate) missed: u64,
    /// What the event log did not get.
    pub(crate) log: Option<String>,
}

pub(crate) fn summarize(report: &ShutdownReport) -> Summary {
    Summary {
        verdict: report.verdict(),
        closed: match &report.closed_by {
            harness::ClosedBy::Owner => None,
            by => Some(by.to_string()),
        },
        not_stopped: match &report.stopped {
            Stopped::NotProven { reason, .. } => Some(reason.clone()),
            _ => None,
        },
        executions: report
            .executions
            .iter()
            .map(|execution| (execution.id.get(), execution.status))
            .collect(),
        hosted: report
            .hosted
            .iter()
            .map(|call| {
                let outcome = serde_json::to_value(call.outcome)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default();
                let reason = call
                    .reason
                    .and_then(|reason| serde_json::to_value(reason).ok())
                    .and_then(|v| v.as_str().map(|r| format!(" ({})", r.replace('_', " "))))
                    .unwrap_or_default();
                format!(
                    "call {} to binding {}: {outcome}{reason}",
                    call.request, call.binding
                )
            })
            .collect(),
        last_sequence: report.events.last_sequence,
        missed: report.events.missed.first().copied().unwrap_or(0),
        log: report.events.log.as_ref().map(ToString::to_string),
    }
}

/// The report, on stderr: whether the session is stopped, how each execution
/// running at the close ended, how each binding call that ended after the
/// close ended, and anything its events lost.
pub(crate) fn render_report(summary: &Summary) -> String {
    let mut text = String::from(match summary.verdict {
        Verdict::Clean => "[outrig] session closed: stopped, every outcome known\n",
        Verdict::StoppedWithUnknown => {
            "[outrig] session closed: stopped, but some Python or binding call was cut off -- what \
             it did may or may not have happened\n"
        }
        _ => "[outrig] session closed: NOT proven stopped -- something it started may still be \
              running\n",
    });
    if let Some(by) = &summary.closed {
        text.push_str(&format!("[outrig]   closed because {by}\n"));
    }
    if let Some(reason) = &summary.not_stopped {
        text.push_str(&format!("[outrig]   not proven stopped: {reason}\n"));
    }
    for (id, status) in &summary.executions {
        let status = match status {
            ExecutionStatus::Ok => "ok",
            ExecutionStatus::Error => "error",
            _ => "unknown",
        };
        text.push_str(&format!("[outrig]   execution {id}: {status}\n"));
    }
    for call in &summary.hosted {
        text.push_str(&format!("[outrig]   {call}\n"));
    }
    if summary.missed > 0 {
        text.push_str(&format!(
            "[outrig]   events: {} published; {} were not shown here\n",
            summary.last_sequence, summary.missed
        ));
    }
    if let Some(log) = &summary.log {
        text.push_str(&format!("[outrig] warning: {log}\n"));
    }
    text
}

/// What the terminal says of a session a host program left for the engine to stop: the report
/// of one it shut down, or whether what the start of one that never finished starting made is
/// stopped.
pub(crate) fn render_left_open(left: &LeftOpen) -> String {
    match &left.end {
        SessionEnd::ShutDown(report) => {
            let mut summary = summarize(report);
            // That subscription was the program's, and nothing is left to have read it.
            summary.missed = 0;
            format!(
                "[outrig] session {} was open when the program ended, and was shut down\n{}",
                left.id,
                render_report(&summary)
            )
        }
        SessionEnd::NotStarted(Stopped::Proven) => format!(
            "[outrig] session {} never finished starting; what its start made is stopped\n",
            left.id
        ),
        SessionEnd::NotStarted(Stopped::NotProven { reason, .. }) => format!(
            "[outrig] session {} never finished starting, and what its start made is NOT proven \
             stopped: {reason}\n",
            left.id
        ),
        // A kind of end this build does not know is not one it can call stopped.
        _ => format!(
            "[outrig] session {} was left open, and is NOT proven stopped\n",
            left.id
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary of a report that `verdict` reads as, and nothing else.
    fn summary(verdict: Verdict) -> Summary {
        Summary {
            verdict,
            closed: None,
            not_stopped: None,
            executions: Vec::new(),
            hosted: Vec::new(),
            last_sequence: 0,
            missed: 0,
            log: None,
        }
    }

    /// What the terminal says of each kind of report.
    #[test]
    fn the_report_says_what_stopped_and_what_was_cut_off() {
        assert_eq!(
            render_report(&Summary {
                last_sequence: 40,
                ..summary(Verdict::Clean)
            }),
            "[outrig] session closed: stopped, every outcome known\n"
        );

        let cut = render_report(&Summary {
            executions: vec![(7, ExecutionStatus::Unknown), (9, ExecutionStatus::Ok)],
            hosted: vec!["call 4 to binding repo: unknown (killed)".to_string()],
            last_sequence: 80,
            missed: 3,
            ..summary(Verdict::StoppedWithUnknown)
        });
        assert!(cut.starts_with(
            "[outrig] session closed: stopped, but some Python or binding call was cut off"
        ));
        assert!(cut.contains("[outrig]   execution 7: unknown\n"), "{cut}");
        assert!(
            cut.contains("[outrig]   call 4 to binding repo: unknown (killed)\n"),
            "{cut}"
        );
        assert!(cut.contains("[outrig]   execution 9: ok\n"), "{cut}");
        assert!(cut.contains("80 published; 3 were not shown here"), "{cut}");

        let stuck = render_report(&Summary {
            closed: Some("the Python interpreter exited (exit status: 1)".to_string()),
            not_stopped: Some("the primary container: timed out".to_string()),
            log: Some("2 agent event(s) could not be written".to_string()),
            ..summary(Verdict::NotProvenStopped)
        });
        assert!(stuck.starts_with("[outrig] session closed: NOT proven stopped"), "{stuck}");
        assert!(
            stuck.contains("closed because the Python interpreter exited (exit status: 1)"),
            "{stuck}"
        );
        assert!(stuck.contains("not proven stopped: the primary container: timed out"));
        assert!(stuck.contains("[outrig] warning: 2 agent event(s)"), "{stuck}");
    }
}
