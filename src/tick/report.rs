//! What one tick says about itself: the [`Report`] it returns and the one
//! line [`Report::summary`] turns it into for the operator.

/// What one tick did.
///
/// Every field is a count of something an operator might go looking for
/// afterwards, which is why the two "left alone" counts are separate: one
/// says a rename was refused, the other says a compression or a deletion
/// was, and they have different causes.
///
/// `Debug` is derived, deliberately, for the reason [`Error`](crate::error::Error) gives: every
/// message in the fault lists is printed for the operator on the summary
/// line, and the path in it is the diagnostic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Live log paths renamed. Counts paths, not sheep: two sheep sharing
    /// one path rotate it once between them.
    pub rotated: usize,
    /// Log paths this tick could not consider at all, because they could not
    /// be stat'd or could not be spelled. A sheep registered but never
    /// started has no log file yet, and that is the ordinary case here
    /// rather than a fault. A file that was merely too small to qualify is
    /// not counted: that is not a skip, it is the answer.
    pub skipped: usize,
    /// Generations gzipped.
    pub compressed: usize,
    /// Files deleted for being past `keep`.
    ///
    /// Files rather than generations, because the two can differ. A
    /// generation an earlier crash left half-compressed wears two files, and
    /// they go together, so it contributes two. Assigned straight from
    /// [`Tidied::deleted`](crate::prune::Tidied::deleted), so it carries
    /// that field's unit and says so here rather than leaving the two types
    /// disagreeing across the seam.
    pub deleted: usize,
    /// Logs left unrotated because a name this base rotates into is some
    /// sheep's live log. See the module docs. Anything above zero wants a
    /// human: the two logs need different names before either can rotate.
    pub skipped_collision: usize,
    /// Times `tidy` refused to compress or delete a generation for the same
    /// reason.
    ///
    /// Refusals rather than files: one file can be the reason for two of
    /// them, once as a candidate generation of its own and once as the name
    /// some other generation wanted to compress into. Assigned straight from
    /// [`Tidied::skipped_protected`](crate::prune::Tidied::skipped_protected),
    /// so it carries that field's unit.
    pub skipped_protected: usize,
    /// The sheep whose reopen was refused, and what it said. Set means the
    /// tick stopped early: no further sheep was rotated, and nothing that
    /// sheep or a later one writes to was compressed or pruned. What the
    /// sheep before it had reopened was still tidied.
    pub reopen_failed: Option<String>,
    /// Renames that failed, one message each. A rename that fails leaves
    /// the live file where it was, so nothing is lost and that log is simply
    /// not rotated this tick; the next log is. The sheep is reopened only if
    /// something else of its did rotate, and a sheep not reopened has every
    /// path it writes to left alone by tidy. The fault recurs next tick if
    /// it persists.
    pub rename_failed: Vec<String>,
    /// Compressions and deletions that failed, one message each. The
    /// generation stays as it was: a compression removes the plain file
    /// only after the `.gz` is complete and synced. The next base is still
    /// tidied.
    pub tidy_failed: Vec<String>,
    /// Log directories that could not be listed to read a log's age, one
    /// message each. The logs under `max_size` in them are left for a later
    /// tick; one over it still rotates, and then its tidy fails to list the
    /// same directory and says so too. Anything here wants a human: it is a
    /// permissions problem.
    pub unlistable: Vec<String>,
    /// Sheep left alone this tick because their own `[app.dogs.<name>]`
    /// table could not be used, one message each naming the sheep and the
    /// key. Their logs are neither rotated nor tidied, and a log they share
    /// with another sheep is left alone for it too. The fault recurs next
    /// tick until somebody edits the table.
    pub left_alone: Vec<String>,
}

impl Report {
    /// The one line worth printing for this tick, or `None` when there is
    /// nothing to say.
    ///
    /// This dog's own log is one of the files it rotates, so silence on a
    /// quiet tick is not tidiness, it is the difference between a log
    /// directory this dog keeps small and one it fills. A tick that only
    /// looked says nothing; a tick that renamed something, refused to, or
    /// was refused a reopen says one line.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if self.rotated == 0
            && self.skipped_collision == 0
            && self.reopen_failed.is_none()
            && self.rename_failed.is_empty()
            && self.tidy_failed.is_empty()
            && self.unlistable.is_empty()
            && self.left_alone.is_empty()
        {
            return None;
        }
        let mut line = format!(
            "rotated {}, compressed {}, deleted {}",
            self.rotated, self.compressed, self.deleted
        );
        if self.skipped_collision > 0 {
            line.push_str(&format!(
                ", left {} alone whose rotated name is a live log",
                self.skipped_collision
            ));
        }
        if self.skipped_protected > 0 {
            line.push_str(&format!(
                ", refused {} to touch a live log",
                times(self.skipped_protected)
            ));
        }
        if let Some(why) = &self.reopen_failed {
            line.push_str(&format!(", stopped early: reopen refused ({why})"));
        }
        for why in &self.left_alone {
            line.push_str(&format!(", left {why}"));
        }
        for (what, faults) in [
            ("rename", &self.rename_failed),
            ("compress or delete", &self.tidy_failed),
            ("list a log directory", &self.unlistable),
        ] {
            if let Some(first) = faults.first() {
                line.push_str(&format!(
                    ", failed {} to {what} ({first})",
                    times(faults.len())
                ));
            }
        }
        Some(line)
    }
}

/// `once`, `twice`, or `n times`, for a summary line a person reads.
fn times(n: usize) -> String {
    match n {
        1 => "once".to_owned(),
        2 => "twice".to_owned(),
        n => format!("{n} times"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::assert_no_dashes;

    #[test]
    fn a_quiet_tick_says_nothing_and_a_busy_one_says_one_line() {
        // This dog's own logs are in the flock it rotates, so a line per
        // file per interval would make its log the busiest file in the
        // directory it exists to keep small.
        let quiet = Report {
            skipped: 3,
            ..Report::default()
        };
        assert_eq!(quiet.summary(), None);

        let busy = Report {
            rotated: 2,
            compressed: 1,
            ..Report::default()
        };
        let line = busy.summary().expect("a tick that rotated says so");
        assert_eq!(line.lines().count(), 1, "{line}");
        assert_no_dashes(&line);
    }
}
