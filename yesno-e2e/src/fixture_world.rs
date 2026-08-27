include!("scenario_prefix.rs");

use std::path::PathBuf;

use monty_types::{MontyException, MontyObject};

use crate::convert::{value_err, Args};

pub fn all_names() -> &'static [&'static str] {
    crate::fixture::OWNS
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HandleKind {
    FixtureProcess,
}

impl HandleKind {
    fn name(self) -> &'static str {
        match self {
            Self::FixtureProcess => "fixture process",
        }
    }
}

pub struct World {
    pub(crate) root: PathBuf,
    pub(crate) fixture: crate::fixture::FixtureState,
    _tmp: tempfile::TempDir,
    handles: Vec<(HandleKind, usize)>,
    calls: u64,
}

impl World {
    pub fn temporary_with(args: Vec<(String, u64)>) -> std::io::Result<Self> {
        Self::temporary_labelled("adhoc", args)
    }

    /// [`Self::temporary_with`], naming the root after the scenario that owns it.
    pub fn temporary_labelled(label: &str, _args: Vec<(String, u64)>) -> std::io::Result<Self> {
        let tmp = tempfile::Builder::new()
            .prefix(&scenario_prefix(label))
            .tempdir()?;
        Ok(Self {
            root: tmp.path().to_path_buf(),
            fixture: Default::default(),
            _tmp: tmp,
            handles: Vec::new(),
            calls: 0,
        })
    }

    pub fn is_verb(name: &str) -> bool {
        all_names().contains(&name)
    }

    pub fn call(
        &mut self,
        verb: &str,
        pos: &[MontyObject],
        kw: &[(MontyObject, MontyObject)],
    ) -> Result<MontyObject, MontyException> {
        self.calls += 1;
        self.call_fixture(verb, &Args::new(verb, pos, kw))
    }

    pub fn calls(&self) -> u64 {
        self.calls
    }

    pub(crate) fn mint(&mut self, kind: HandleKind, index: usize) -> MontyObject {
        self.handles.push((kind, index));
        MontyObject::Int((self.handles.len() - 1) as i64)
    }

    pub(crate) fn slot(
        &self,
        handle: usize,
        want: HandleKind,
        verb: &str,
    ) -> Result<usize, MontyException> {
        let Some(&(got, index)) = self.handles.get(handle) else {
            return Err(value_err(format!(
                "{verb}(): handle {handle} was never issued by this scenario"
            )));
        };
        if got != want {
            return Err(value_err(format!(
                "{verb}(): handle {handle} is a {}, not a {}",
                got.name(),
                want.name()
            )));
        }
        Ok(index)
    }
}
