use std::io;

use duded::Settings;
use dudes_journal::Reader;
use dudes_proto::Entry;
use dudes_proto::rpc::hook::HookToken;
use dudes_supervisor::Rng;
use dudes_supervisor::testing::{FakeRng, FakeTokens};
use dudes_types::Lsn;
use ulid::Ulid;

#[derive(Clone, Debug)]
pub(crate) struct FixtureRng(FakeRng);

impl FixtureRng {
    pub(crate) fn from_rng(settings: &Settings, mut initial: FakeRng) -> Self {
        let dir = settings.state_dir.join(duded::boot::JOURNAL_DIR);
        let entries: Vec<Entry> = match std::fs::symlink_metadata(&dir) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("preparing fixture RNG from {}: {error}", dir.display()),
            Ok(_) => {
                let (reader, _) = Reader::open(&dir).unwrap_or_else(|error| {
                    panic!("preparing fixture RNG from {}: {error}", dir.display())
                });
                reader
                    .iter_from(Lsn::ZERO)
                    .unwrap_or_else(|error| {
                        panic!("preparing fixture RNG from {}: {error}", dir.display())
                    })
                    .map(|entry| {
                        entry
                            .unwrap_or_else(|error| {
                                panic!("preparing fixture RNG from {}: {error}", dir.display())
                            })
                            .1
                    })
                    .collect()
            }
        };
        initial
            .reserve_entries(&entries)
            .expect("fixture entries serialize");
        Self(initial)
    }

    pub(crate) fn tokens(&self) -> FakeTokens {
        self.0.tokens()
    }

    pub(crate) fn drawn(&self) -> u128 {
        self.0.drawn()
    }
}

impl Rng for FixtureRng {
    fn ulid(&mut self) -> Ulid {
        self.0.ulid()
    }
    fn agent_token(&mut self) -> HookToken {
        self.0.agent_token()
    }
}
