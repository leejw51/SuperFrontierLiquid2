//! A checkpoint log, so a long journey reports every break rather than the first.
//!
//! The product journey is one test on purpose — sign in, claim, rest, cancel,
//! cross — because the thing being tested is the sequence, and a suite of five
//! independent tests would not notice that step four only works when step three
//! was skipped.
//!
//! But a plain `assert!` in the middle of a sequence hides everything after it.
//! If the balance view is wrong at step three, whoever reads the failure learns
//! nothing about whether the fill at step six works, and finds out one fix at a
//! time. So observations are recorded here and the test fails once, at the end,
//! listing every observation that did not hold.
//!
//! This is not a softer assertion. A journal with any entry fails the test, and
//! anything structural — no session token, no order id — still panics on the
//! spot, because the rest of the journey would be meaningless without it.

/// Checkpoints recorded during one journey.
pub struct Journal {
    name: String,
    steps: Vec<Step>,
}

struct Step {
    description: String,
    failure: Option<String>,
}

impl Journal {
    /// A journal for a named journey.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            steps: Vec::new(),
        }
    }

    /// Records a checkpoint that held.
    pub fn passed(&mut self, description: impl Into<String>) {
        self.steps.push(Step {
            description: description.into(),
            failure: None,
        });
    }

    /// Records a checkpoint that did not hold, with what was seen instead.
    pub fn failed(&mut self, description: impl Into<String>, detail: impl Into<String>) {
        self.steps.push(Step {
            description: description.into(),
            failure: Some(detail.into()),
        });
    }

    /// Records a checkpoint, passing or failing on `held`.
    pub fn check(&mut self, held: bool, description: impl Into<String>, detail: impl Into<String>) {
        if held {
            self.passed(description);
        } else {
            self.failed(description, detail);
        }
    }

    /// Compares two values, recording the difference when they differ.
    pub fn check_eq<T: PartialEq + std::fmt::Display>(
        &mut self,
        actual: T,
        expected: T,
        description: impl Into<String>,
    ) {
        let held = actual == expected;
        let detail = format!("expected {expected}, got {actual}");
        self.check(held, description, detail);
    }

    /// Fails the test if any checkpoint did not hold, printing the whole
    /// journey in order so the failure reads as a narrative.
    pub fn finish(self) {
        let broken = self.steps.iter().filter(|s| s.failure.is_some()).count();
        if broken == 0 {
            return;
        }
        let mut report = format!(
            "\n{}: {broken} of {} checkpoints did not hold.\n\n",
            self.name,
            self.steps.len()
        );
        for step in &self.steps {
            match &step.failure {
                None => report.push_str(&format!("  ok    {}\n", step.description)),
                Some(detail) => report.push_str(&format!(
                    "  BROKE {}\n          {detail}\n",
                    step.description
                )),
            }
        }
        panic!("{report}");
    }
}
