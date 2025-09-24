mod lua;

pub use lua::LuaRunner;

#[derive(Debug, Clone)]
pub enum ScriptStep {
    Filter(String),
    Map(String),
}
