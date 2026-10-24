//! Bounded reads of one agent's memory for a viewer: a page of entries newest first, one entry by
//! position, the active summary and the withheld appends. Each read copies at most what it returns.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::ops::Bound::{Excluded, Unbounded};

use dudes_proto::PeerProvenance;
use dudes_types::{AgentId, Lsn, Timestamp};

use super::MemoryProjection;

/// How many withheld appends each agent keeps markers for. Older ones are only counted.
const WITHHELD_KEPT: usize = 8;

/// One applied entry as a viewer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MemoryEntry {
    /// The append's journal position.
    pub lsn: Lsn,
    /// Its entry's time, a label only.
    pub at: Timestamp,
    /// The provenance the append recorded, absent on a record written before it was recorded.
    pub provenance: Option<PeerProvenance>,
    /// Whether the active summary covers it.
    pub condensed: bool,
    /// The whole entry's decoded length in bytes.
    pub len: u64,
    /// The entry, or its first bytes up to the read's bound, cut at a character boundary.
    pub text: String,
    /// Whether `text` is shorter than the entry.
    pub cut: bool,
}

/// One page of an agent's applied entries.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MemoryPage {
    /// How many applied entries the agent has.
    pub count: u64,
    /// The last position the active summary covers.
    pub through: Option<Lsn>,
    /// Entries below the page's cursor, newest first.
    pub entries: Vec<MemoryEntry>,
    /// Whether an older applied entry exists below the last one returned.
    pub more: bool,
}

/// What made the active summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemorySummaryKind {
    /// Exact excerpts the daemon cut from the entries.
    Excerpts,
    /// A model's prose about the entries.
    Model,
}

/// The active summary as a viewer reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MemorySummary {
    /// The summary record's journal position.
    pub lsn: Lsn,
    /// Its entry's time, a label only.
    pub at: Timestamp,
    /// What made it.
    pub kind: MemorySummaryKind,
    /// How many entries it covers.
    pub sections: u64,
    /// How many bytes of entries it covers.
    pub bytes: u64,
    /// Its rendered text, the text a launch recall carries.
    pub text: String,
}

/// One append that was withheld from memory. Its bytes were never kept.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct WithheldAppend {
    /// The quarantine record's journal position.
    pub lsn: Lsn,
    /// Its entry's time, a label only.
    pub at: Timestamp,
    /// The withheld text's decoded length in bytes.
    pub len: u64,
    /// The writer's resolved provenance.
    pub provenance: PeerProvenance,
    /// Why it was withheld, as the record states it.
    pub reason: String,
}

/// An agent's withheld appends: how many there are, and the newest of them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Withheld {
    /// How many appends to this agent's memory were withheld.
    pub count: u64,
    /// The newest withheld appends, newest first, at most eight.
    pub newest: Vec<WithheldAppend>,
}

#[derive(Debug, Default)]
pub(super) struct WithheldLog {
    count: u64,
    newest: VecDeque<WithheldAppend>,
}

impl WithheldLog {
    pub(super) fn fold(
        &mut self,
        lsn: Lsn,
        at: Timestamp,
        len: u64,
        provenance: PeerProvenance,
        reason: &str,
    ) {
        if self.newest.front().is_some_and(|newest| newest.lsn >= lsn) {
            return;
        }
        self.count = self.count.saturating_add(1);
        self.newest.push_front(WithheldAppend {
            lsn,
            at,
            len,
            provenance,
            reason: reason.to_owned(),
        });
        self.newest.truncate(WITHHELD_KEPT);
    }
}

fn prefix(text: &str, max: usize) -> (&str, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

impl MemoryProjection {
    fn condensed_through(&self, agent: AgentId) -> Option<Lsn> {
        self.summaries
            .get(&agent)
            .map(|summary| summary.envelope.to_lsn)
    }

    fn entry_view(
        through: Option<Lsn>,
        lsn: Lsn,
        applied: &super::Applied,
        max: usize,
        scrub: &dyn for<'t> Fn(&'t str) -> Cow<'t, str>,
    ) -> MemoryEntry {
        let scrubbed = scrub(&applied.text);
        let (text, _) = prefix(&scrubbed, max);
        let cut = applied.text.len() > max;
        MemoryEntry {
            lsn,
            at: applied.at,
            provenance: applied.provenance,
            condensed: through.is_some_and(|through| lsn <= through),
            len: u64::try_from(applied.text.len()).expect("usize fits into u64"),
            text: text.to_owned(),
            cut,
        }
    }

    /// Up to `limit` applied entries below `before` (all of them when `None`), newest first, each
    /// scrubbed and then cut to at most `preview` bytes. `scrub` sees the whole text, so a secret
    /// that straddles the cut is replaced, never half shown.
    #[must_use]
    pub fn page(
        &self,
        agent: AgentId,
        before: Option<Lsn>,
        limit: usize,
        preview: usize,
        scrub: &dyn for<'t> Fn(&'t str) -> Cow<'t, str>,
    ) -> MemoryPage {
        let through = self.condensed_through(agent);
        let Some(entries) = self.entries.get(&agent) else {
            return MemoryPage {
                count: 0,
                through,
                entries: Vec::new(),
                more: false,
            };
        };
        let mut below = entries
            .range((Unbounded, before.map_or(Unbounded, Excluded)))
            .rev();
        let page = below
            .by_ref()
            .take(limit)
            .map(|(lsn, applied)| Self::entry_view(through, *lsn, applied, preview, scrub))
            .collect();
        MemoryPage {
            count: u64::try_from(entries.len()).expect("usize fits into u64"),
            through,
            entries: page,
            more: below.next().is_some(),
        }
    }

    /// The applied entry at `lsn`, scrubbed and then cut to at most `max` bytes, or `None` when
    /// this agent has no applied entry there.
    #[must_use]
    pub fn entry(
        &self,
        agent: AgentId,
        lsn: Lsn,
        max: usize,
        scrub: &dyn for<'t> Fn(&'t str) -> Cow<'t, str>,
    ) -> Option<MemoryEntry> {
        let applied = self.entries.get(&agent)?.get(&lsn)?;
        Some(Self::entry_view(
            self.condensed_through(agent),
            lsn,
            applied,
            max,
            scrub,
        ))
    }

    /// The active summary, when the agent's memory has been condensed.
    #[must_use]
    pub fn summary(&self, agent: AgentId) -> Option<MemorySummary> {
        let summary = self.summaries.get(&agent)?;
        let (sections, bytes) = summary.covered();
        Some(MemorySummary {
            lsn: summary.record_lsn,
            at: summary.at,
            kind: summary.kind(),
            sections,
            bytes,
            text: summary.rendered.clone(),
        })
    }

    /// The agent's withheld appends.
    #[must_use]
    pub fn withheld(&self, agent: AgentId) -> Withheld {
        self.withheld
            .get(&agent)
            .map_or_else(Withheld::default, |log| Withheld {
                count: log.count,
                newest: log.newest.iter().cloned().collect(),
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefix_ends_on_a_character_boundary_at_or_under_its_bound() {
        assert_eq!(prefix("abc", 3), ("abc", false));
        assert_eq!(prefix("a🦀", 4), ("a", true));
        assert_eq!(prefix("a🦀", 5), ("a🦀", false));
        assert_eq!(prefix("ab🦀", 5), ("ab", true));
    }
}
