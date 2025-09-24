use std::{
    cell::RefCell,
    collections::hash_map::Entry,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread::ThreadId,
};

use fnv::{FnvHashMap, FnvHashSet};
use quickwit_script::{LuaRunner, ScriptStep};
use serde_json::Value;

thread_local! {
    static THREAD_MAP: RefCell<FnvHashMap<usize, LuaRunner>> = RefCell::new(FnvHashMap::default());
}

fn global_counter() -> &'static AtomicUsize {
    static COUNTER: OnceLock<AtomicUsize> = OnceLock::new();
    COUNTER.get_or_init(|| AtomicUsize::new(0))
}

fn dead_ids() -> &'static Mutex<FnvHashMap<usize, FnvHashSet<ThreadId>>> {
    static DEAD_IDS: OnceLock<Mutex<FnvHashMap<usize, FnvHashSet<ThreadId>>>> = OnceLock::new();
    DEAD_IDS.get_or_init(|| Mutex::new(FnvHashMap::default()))
}

fn cleanup_dead_ids(map: &mut FnvHashMap<usize, LuaRunner>) {
    let thread_id = std::thread::current().id();
    let mut dead = dead_ids().lock().unwrap();

    let mut to_remove_ids = Vec::new();

    for (&id, threads) in dead.iter_mut() {
        if threads.remove(&thread_id) {
            map.remove(&id);
        }
        if threads.is_empty() {
            to_remove_ids.push(id);
        }
    }

    for id in to_remove_ids {
        dead.remove(&id);
    }
}

/// Transform a document by running lua code on it.
/// Runs directly on the current thread. Each thread lazily creates its Lua VM when first needed.
pub struct LuaTransformer {
    id: usize,
    script_steps: Vec<ScriptStep>,
}

impl Drop for LuaTransformer {
    fn drop(&mut self) {
        let thread_id = std::thread::current().id();
        let mut dead = dead_ids().lock().unwrap();
        dead.entry(self.id).or_default().insert(thread_id);
    }
}

impl LuaTransformer {
    /// Create a new lua transformer.
    pub fn new(script: Vec<quickwit_proto::search::ScriptStep>) -> anyhow::Result<Self> {
        use quickwit_proto::search::script_step::ScriptStep as ProtoScriptStep;

        let script_steps: Vec<ScriptStep> = script
            .into_iter()
            .map(|s| match s.script_step {
                Some(ProtoScriptStep::Filter(filter)) => Ok(ScriptStep::Filter(filter)),
                Some(ProtoScriptStep::Map(map)) => Ok(ScriptStep::Map(map)),
                None => Err(anyhow::anyhow!("ScriptStep must have a script_step field")),
            })
            .collect::<anyhow::Result<_>>()?;

        let id = global_counter().fetch_add(1, Ordering::Relaxed);

        Ok(Self { id, script_steps })
    }

    fn get_or_create_runner<'a>(
        &self,
        map: &'a mut FnvHashMap<usize, LuaRunner>,
    ) -> anyhow::Result<(&'a mut LuaRunner, bool)> {
        let mut is_new = false;

        let runner = match map.entry(self.id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                is_new = true;
                let runner = LuaRunner::new(self.script_steps.clone())
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                entry.insert(runner)
            }
        };

        Ok((runner, is_new))
    }

    /// Run a transform on the current thread.
    /// If the Lua VM hasn't been initialized yet for this transformer on this thread, build it.
    pub fn transform(&self, input_doc: Value) -> anyhow::Result<Option<Value>> {
        THREAD_MAP.with(|map_cell| {
            let mut map = map_cell.borrow_mut();

            let (runner, is_new) = self.get_or_create_runner(&mut map)?;

            let result = runner
                .run(input_doc)
                .map_err(|e| anyhow::anyhow!(e.to_string()));

            if is_new {
                cleanup_dead_ids(&mut map);
            }

            result
        })
    }
}
