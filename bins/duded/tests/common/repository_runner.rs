#![cfg(test)]

use std::ffi::OsStr;
use std::io;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use duded::predicate::{PredicateOutcome, PredicateRunner};
use dudes_host::{OneShot, OneShotOutcome};
use dudes_providers::launch::LaunchSpec;

type AddCut = Box<dyn FnOnce() + Send>;

#[derive(Default)]
pub(crate) struct Runner {
    inner: OneShot,
    pub(crate) adds: AtomicUsize,
    cut: Mutex<Option<(usize, bool, AddCut)>>,
}

impl Runner {
    pub(crate) fn after_add(&self, nth: usize, fail: bool, cut: impl FnOnce() + Send + 'static) {
        *self.cut.lock().unwrap() = Some((nth, fail, Box::new(cut)));
    }
}

impl PredicateRunner for Runner {
    fn run(&self, spec: &LaunchSpec, timeout: Duration) -> io::Result<PredicateOutcome> {
        let result = self.inner.run(spec, timeout)?;
        if spec
            .argv
            .windows(2)
            .any(|args| args == [OsStr::new("worktree"), OsStr::new("add")])
        {
            let nth = self.adds.fetch_add(1, Ordering::SeqCst) + 1;
            let cut = {
                let mut cut = self.cut.lock().unwrap();
                if cut.as_ref().is_some_and(|(at, _, _)| *at == nth) {
                    cut.take()
                } else {
                    None
                }
            };
            if let Some((_, fail, cut)) = cut {
                cut();
                if fail {
                    return Ok(PredicateOutcome::Finished {
                        exit: Some(1),
                        output_tail: "injected failure after completed add".into(),
                        timed_out: false,
                    });
                }
            }
        }
        Ok(match result {
            OneShotOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            } => PredicateOutcome::Finished {
                exit,
                output_tail,
                timed_out,
            },
            OneShotOutcome::Swept => PredicateOutcome::Swept,
        })
    }

    fn sweep(&self) {
        self.inner.sweep();
    }

    fn run_captured(
        &self,
        spec: &LaunchSpec,
        timeout: Duration,
        cap: usize,
    ) -> io::Result<duded::predicate::PredicateCaptured> {
        self.inner
            .run_captured(spec, timeout, cap)
            .map(|result| match result {
                dudes_host::CapturedOutcome::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                } => duded::predicate::PredicateCaptured::Finished {
                    exit,
                    stdout,
                    truncated,
                    timed_out,
                },
                dudes_host::CapturedOutcome::Swept => duded::predicate::PredicateCaptured::Swept,
            })
    }
}
