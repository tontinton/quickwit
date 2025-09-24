use std::cell::{Ref, RefCell, RefMut};
use std::rc::Rc;

use mlua::{
    Function as LuaFunction, Lua, MetaMethod, Result, UserData, UserDataMethods, Value as LuaValue,
};
use serde_json::Value;

use crate::ScriptStep;

enum LuaRunnerStep {
    Filter(LuaFunction),
    Map(LuaFunction),
}

pub struct LuaRunner {
    lua: Lua,
    steps: Vec<LuaRunnerStep>,
}

impl LuaRunner {
    pub fn new(script_steps: Vec<ScriptStep>) -> Result<Self> {
        let lua = Lua::new();

        lua.load(
            r#"
local original_pairs = pairs
function pairs(t)
    if type(t) == "userdata" and t.__pairs_impl then
        local ok, iter, state, key = pcall(function() return t:__pairs_impl() end)
        if ok then return iter, state, key end
    end
    return original_pairs(t)
end
        "#,
        )
        .exec()?;

        let mut steps = Vec::with_capacity(script_steps.len());
        for (i, step) in script_steps.into_iter().enumerate() {
            match step {
                ScriptStep::Filter(code) => {
                    steps.push(LuaRunnerStep::Filter(compile_lua_fn(&lua, i, &code)?))
                }
                ScriptStep::Map(code) => {
                    steps.push(LuaRunnerStep::Map(compile_lua_fn(&lua, i, &code)?))
                }
            }
        }

        Ok(Self { lua, steps })
    }

    pub fn run(&self, input_doc: Value) -> Result<Option<Value>> {
        let mut lua_doc = json_to_lua(&self.lua, input_doc)?;

        for step in &self.steps {
            match step {
                LuaRunnerStep::Filter(function) => {
                    let keep = function.call::<bool>(&lua_doc)?;
                    if !keep {
                        return Ok(None);
                    }
                }
                LuaRunnerStep::Map(function) => {
                    let ret_val = function.call::<LuaValue>(lua_doc)?;
                    let output_doc = match ret_val {
                        LuaValue::UserData(data) => data
                            .borrow::<SharedValue>()
                            .map_or(Value::Null, |v| v.clone().take()),
                        _ => lua_to_json(ret_val)?,
                    };
                    lua_doc = json_to_lua(&self.lua, output_doc)?;
                }
            }
        }

        Ok(Some(match lua_doc {
            LuaValue::UserData(data) => data
                .borrow::<SharedValue>()
                .map_or(Value::Null, |v| v.clone().take()),
            _ => lua_to_json(lua_doc)?,
        }))
    }
}

fn compile_lua_fn(lua: &Lua, idx: usize, code: &str) -> Result<LuaFunction> {
    lua.load(format!(
        r#"
local function f{idx}(doc)
    {code}
end
return f{idx}
                "#
    ))
    .eval()
}

#[derive(Clone)]
struct SharedValue {
    root: Rc<RefCell<Value>>,
    path: Vec<PathElement>,
}

#[derive(Clone)]
enum PathElement {
    Key(String),
    Index(usize),
}

impl SharedValue {
    fn new(root: Value) -> Self {
        Self {
            root: Rc::new(RefCell::new(root)),
            path: Vec::new(),
        }
    }

    fn take(self) -> Value {
        let mut node = self.root.take();
        for elem in &self.path {
            node = match elem {
                PathElement::Key(k) => remove_by_key(node, k).unwrap(),
                PathElement::Index(i) => remove_by_index(node, *i).unwrap(),
            };
        }
        node
    }

    fn resolve(&self) -> Ref<'_, Value> {
        let mut node = self.root.borrow();
        for elem in &self.path {
            node = match elem {
                PathElement::Key(k) => Ref::filter_map(node, |n| n.get(k)).unwrap(),
                PathElement::Index(i) => Ref::filter_map(node, |n| n.get(*i)).unwrap(),
            };
        }
        node
    }

    fn resolve_mut(&self) -> RefMut<'_, Value> {
        let mut node = self.root.borrow_mut();
        for elem in &self.path {
            node = match elem {
                PathElement::Key(k) => RefMut::filter_map(node, |n| n.get_mut(k)).unwrap(),
                PathElement::Index(i) => RefMut::filter_map(node, |n| n.get_mut(*i)).unwrap(),
            };
        }
        node
    }

    fn subhandle(&self, elem: PathElement) -> Self {
        let mut new_path = self.path.clone();
        new_path.push(elem);
        Self {
            root: self.root.clone(),
            path: new_path,
        }
    }
}

fn remove_by_key(value: Value, key: &str) -> Option<Value> {
    if let Value::Object(mut map) = value {
        map.remove(key)
    } else {
        None
    }
}

fn remove_by_index(value: Value, index: usize) -> Option<Value> {
    match value {
        Value::Array(mut arr) if index < arr.len() => Some(arr.remove(index)),
        _ => None,
    }
}

impl UserData for SharedValue {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: LuaValue| {
            let val = this.resolve();
            match key {
                LuaValue::String(s) => {
                    let k = s.to_str()?.to_string();
                    if let Some(child) = val.get(&k) {
                        Ok(json_subhandle_to_lua(
                            lua,
                            this.clone(),
                            child,
                            PathElement::Key(k),
                        )?)
                    } else {
                        Ok(LuaValue::Nil)
                    }
                }
                LuaValue::Integer(i) => {
                    let idx = (i - 1) as usize;
                    if let Some(child) = val.get(idx) {
                        Ok(json_subhandle_to_lua(
                            lua,
                            this.clone(),
                            child,
                            PathElement::Index(idx),
                        )?)
                    } else {
                        Ok(LuaValue::Nil)
                    }
                }
                _ => Ok(LuaValue::Nil),
            }
        });

        methods.add_meta_method_mut(
            MetaMethod::NewIndex,
            |_, this, (key, val): (LuaValue, LuaValue)| {
                let mut node = this.resolve_mut();
                match key {
                    LuaValue::String(s) => {
                        let is_delete = matches!(val, LuaValue::Nil);
                        if is_delete {
                            if let Value::Object(obj) = &mut *node {
                                obj.remove(&*s.to_str()?);
                            }
                        } else {
                            let key_str = s.to_str()?.to_string();
                            node[key_str] = lua_to_json(val)?;
                        }
                    }
                    LuaValue::Integer(i) => {
                        if let Value::Array(arr) = &mut *node {
                            let idx = (i - 1) as usize; // Lua arrays start from 1.
                            if idx < arr.len() {
                                arr[idx] = lua_to_json(val)?;
                            } else if idx == arr.len() {
                                arr.push(lua_to_json(val)?);
                            }
                        }
                    }
                    _ => {}
                }
                Ok(())
            },
        );

        methods.add_meta_method(MetaMethod::Len, |_, this, ()| {
            let val = this.resolve();
            match &*val {
                Value::Array(arr) => Ok(arr.len()),
                Value::Object(obj) => Ok(obj.len()),
                _ => Ok(0),
            }
        });

        methods.add_method("__pairs_impl", |lua, this, ()| {
            let this = this.clone();
            let val = this.resolve().clone();

            match val {
                Value::Object(obj) => make_iter(lua, obj, move |lua, (k, v)| {
                    Ok((
                        LuaValue::String(lua.create_string(&k)?),
                        json_subhandle_to_lua(lua, this.clone(), &v, PathElement::Key(k))?,
                    ))
                }),
                Value::Array(arr) => {
                    make_iter(lua, arr.into_iter().enumerate(), move |lua, (i, v)| {
                        Ok((
                            LuaValue::Integer(i as i64 + 1),
                            json_subhandle_to_lua(lua, this.clone(), &v, PathElement::Index(i))?,
                        ))
                    })
                }
                _ => make_iter(lua, std::iter::empty::<()>(), |_, _| {
                    Ok((LuaValue::Nil, LuaValue::Nil))
                }),
            }
        });
    }
}

fn json_subhandle_to_lua(
    lua: &Lua,
    parent: SharedValue,
    val: &Value,
    elem: PathElement,
) -> Result<LuaValue> {
    Ok(match val {
        Value::Null => LuaValue::Nil,
        Value::Bool(b) => LuaValue::Boolean(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                LuaValue::Integer(i)
            } else {
                LuaValue::Number(n.as_f64().unwrap())
            }
        }
        Value::String(s) => LuaValue::String(lua.create_string(s)?),
        Value::Array(_) | Value::Object(_) => {
            LuaValue::UserData(lua.create_userdata(parent.subhandle(elem))?)
        }
    })
}

fn json_to_lua(lua: &Lua, val: Value) -> Result<LuaValue> {
    Ok(match val {
        Value::Null => LuaValue::Nil,
        Value::Bool(b) => LuaValue::Boolean(b),
        Value::Number(n) => n
            .as_i64()
            .map(LuaValue::Integer)
            .unwrap_or(LuaValue::Number(n.as_f64().unwrap())),
        Value::String(s) => LuaValue::String(lua.create_string(s)?),
        Value::Array(_) | Value::Object(_) => {
            LuaValue::UserData(lua.create_userdata(SharedValue::new(val))?)
        }
    })
}

fn lua_to_json(val: LuaValue) -> Result<Value> {
    Ok(match val {
        LuaValue::Nil => Value::Null,
        LuaValue::Boolean(b) => Value::Bool(b),
        LuaValue::Integer(i) => Value::Number(i.into()),
        LuaValue::Number(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        LuaValue::String(s) => Value::String(s.to_str()?.to_string()),
        LuaValue::UserData(data) => data
            .borrow::<SharedValue>()
            .map_or(Value::Null, |v| v.resolve().clone()),
        LuaValue::Table(t) => {
            let mut arr: Vec<Value> = Vec::new();
            let mut map: serde_json::Map<String, Value> = serde_json::Map::new();
            let mut is_array = true;

            for pair in t.pairs::<LuaValue, LuaValue>() {
                let (k, v) = pair?;
                let value = lua_to_json(v)?;
                match k {
                    LuaValue::Integer(i) if i > 0 => {
                        let idx = (i - 1) as usize;
                        if idx != arr.len() {
                            is_array = false;
                        }
                        if is_array {
                            arr.push(value);
                        } else {
                            map.insert(i.to_string(), value);
                        }
                    }
                    LuaValue::String(s) => {
                        is_array = false;
                        map.insert(s.to_str()?.to_string(), value);
                    }
                    _ => {
                        is_array = false;
                    }
                }
            }

            if is_array && !arr.is_empty() {
                Value::Array(arr)
            } else {
                if !arr.is_empty() {
                    for (i, v) in arr.into_iter().enumerate() {
                        map.insert((i + 1).to_string(), v);
                    }
                }
                Value::Object(map)
            }
        }
        _ => Value::Null,
    })
}

fn make_iter<I, F>(lua: &Lua, iter: I, mut f: F) -> Result<(LuaFunction, LuaValue, LuaValue)>
where
    I: IntoIterator + 'static,
    F: FnMut(&Lua, I::Item) -> Result<(LuaValue, LuaValue)> + 'static,
{
    let mut it = iter.into_iter();
    let iter_fn = lua.create_function_mut(move |lua, _: ()| {
        if let Some(item) = it.next() {
            f(lua, item)
        } else {
            Ok((LuaValue::Nil, LuaValue::Nil))
        }
    })?;
    Ok((iter_fn, LuaValue::Nil, LuaValue::Nil))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_empty_steps() {
        let runner = LuaRunner::new(vec![]).unwrap();
        let input = json!({"name": "test", "value": 42});
        let result = runner.run(input.clone()).unwrap();
        assert_eq!(result, Some(input));
    }

    #[test]
    fn test_simple_filter_pass() {
        let steps = vec![ScriptStep::Filter("return doc.value > 10".to_string())];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"value": 42});
        let result = runner.run(input.clone()).unwrap();
        assert_eq!(result, Some(input));
    }

    #[test]
    fn test_simple_filter_fail() {
        let steps = vec![ScriptStep::Filter("return doc.value > 50".to_string())];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"value": 42});
        let result = runner.run(input).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_multiple_filters() {
        let steps = vec![
            ScriptStep::Filter("return doc.value > 10".to_string()),
            ScriptStep::Filter("return doc.name == 'test'".to_string()),
            ScriptStep::Filter("return doc.active == true".to_string()),
        ];
        let runner = LuaRunner::new(steps).unwrap();

        // Should pass all filters.
        let input = json!({"value": 42, "name": "test", "active": true});
        let result = runner.run(input.clone()).unwrap();
        assert_eq!(result, Some(input));

        // Should fail on second filter.
        let input = json!({"value": 42, "name": "other", "active": true});
        let result = runner.run(input).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_simple_map() {
        let steps = vec![ScriptStep::Map(
            "doc.doubled = doc.value * 2; return doc".to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"value": 21});
        let result = runner.run(input).unwrap();
        let expected = json!({"value": 21, "doubled": 42});
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_map_return_new_object() {
        let steps = vec![ScriptStep::Map(
            "return {original_value = doc.value, computed = doc.value * 3}".to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"value": 10});
        let result = runner.run(input).unwrap();
        let expected = json!({"original_value": 10, "computed": 30});
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_filter_then_map() {
        let steps = vec![
            ScriptStep::Filter("return doc.value > 5".to_string()),
            ScriptStep::Map("doc.category = 'high'; return doc".to_string()),
        ];
        let runner = LuaRunner::new(steps).unwrap();

        // Should pass filter and get mapped.
        let input = json!({"value": 10});
        let result = runner.run(input).unwrap();
        let expected = json!({"value": 10, "category": "high"});
        assert_eq!(result, Some(expected));

        // Should fail filter, no mapping.
        let input = json!({"value": 3});
        let result = runner.run(input).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_map_then_filter() {
        let steps = vec![
            ScriptStep::Map("doc.doubled = doc.value * 2; return doc".to_string()),
            ScriptStep::Filter("return doc.doubled > 20".to_string()),
        ];
        let runner = LuaRunner::new(steps).unwrap();

        let input = json!({"value": 15});
        let result = runner.run(input).unwrap();
        let expected = json!({"value": 15, "doubled": 30});
        assert_eq!(result, Some(expected));

        let input = json!({"value": 5});
        let result = runner.run(input).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_complex_pipeline() {
        let steps = vec![
            ScriptStep::Filter("return doc.status == 'active'".to_string()),
            ScriptStep::Map("doc.processed_at = 'now'; return doc".to_string()),
            ScriptStep::Filter("return doc.priority >= 3".to_string()),
            ScriptStep::Map("return {id = doc.id, urgent = true, data = doc}".to_string()),
        ];
        let runner = LuaRunner::new(steps).unwrap();

        let input = json!({
            "id": "123",
            "status": "active",
            "priority": 5,
            "name": "test"
        });
        let result = runner.run(input.clone()).unwrap();
        let expected = json!({
            "id": "123",
            "urgent": true,
            "data": {
                "id": "123",
                "status": "active",
                "priority": 5,
                "name": "test",
                "processed_at": "now"
            }
        });
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_array_handling() {
        let steps = vec![ScriptStep::Map(
            "doc.count = #doc.items; return doc".to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"items": [1, 2, 3, 4, 5]});
        let result = runner.run(input).unwrap();
        let expected = json!({"items": [1, 2, 3, 4, 5], "count": 5});
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_array_iteration() {
        let steps = vec![ScriptStep::Map(
            r#"
            local sum = 0
            for i, v in pairs(doc.numbers) do
                sum = sum + v
            end
            doc.sum = sum
            return doc
        "#
            .to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"numbers": [10, 20, 30]});
        let result = runner.run(input).unwrap();
        let expected = json!({"numbers": [10, 20, 30], "sum": 60});
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_nested_object_access() {
        let steps = vec![ScriptStep::Map(
            "doc.user_city = doc.user.address.city; return doc".to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({
            "user": {
                "name": "John",
                "address": {
                    "city": "New York",
                    "zip": "10001"
                }
            }
        });
        let result = runner.run(input).unwrap();
        let expected = json!({
            "user": {
                "name": "John",
                "address": {
                    "city": "New York",
                    "zip": "10001"
                }
            },
            "user_city": "New York"
        });
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_nil_values() {
        let steps = vec![ScriptStep::Filter(
            "return doc.optional_field == nil".to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"name": "test"});
        let result = runner.run(input.clone()).unwrap();
        assert_eq!(result, Some(input));
    }

    #[test]
    fn test_type_conversions() {
        let steps = vec![ScriptStep::Map(
            r#"
            return {
                str_val = tostring(doc.number),
                num_val = tonumber(doc.string),
                bool_val = doc.number > 0
            }
        "#
            .to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"number": 42, "string": "123"});
        let result = runner.run(input).unwrap();
        let expected = json!({
            "str_val": "42",
            "num_val": 123,
            "bool_val": true
        });
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_primitive_input() {
        let steps = vec![ScriptStep::Map("return doc * 2".to_string())];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!(21);
        let result = runner.run(input).unwrap();
        assert_eq!(result, Some(json!(42)));
    }

    #[test]
    fn test_string_input() {
        let steps = vec![ScriptStep::Map("return string.upper(doc)".to_string())];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!("hello world");
        let result = runner.run(input).unwrap();
        assert_eq!(result, Some(json!("HELLO WORLD")));
    }

    #[test]
    fn test_boolean_input() {
        let steps = vec![ScriptStep::Filter("return doc == true".to_string())];
        let runner = LuaRunner::new(steps).unwrap();

        let result = runner.run(json!(true)).unwrap();
        assert_eq!(result, Some(json!(true)));

        let result = runner.run(json!(false)).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_array_input() {
        let steps = vec![ScriptStep::Map(
            "doc[#doc + 1] = 'new'; return doc".to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!([1, 2, 3]);
        let result = runner.run(input).unwrap();
        assert_eq!(result, Some(json!([1, 2, 3, "new"])));
    }

    #[test]
    fn test_null_input() {
        let steps = vec![ScriptStep::Filter("return doc == nil".to_string())];
        let runner = LuaRunner::new(steps).unwrap();
        let result = runner.run(json!(null)).unwrap();
        assert_eq!(result, Some(json!(null)));
    }

    #[test]
    fn test_multiple_maps() {
        let steps = vec![
            ScriptStep::Map("return doc * 2".to_string()),
            ScriptStep::Map("return doc + 10".to_string()),
            ScriptStep::Map("return doc / 2".to_string()),
        ];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!(5);
        let result = runner.run(input).unwrap();
        assert_eq!(result, Some(json!(10)));
    }

    #[test]
    fn test_filter_after_failed_filter() {
        let steps = vec![
            ScriptStep::Filter("return doc.value > 100".to_string()),
            ScriptStep::Filter("return doc.value < 50".to_string()),
        ];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"value": 42});
        let result = runner.run(input).unwrap();
        assert_eq!(result, None);
    }

    #[test]
    fn test_error_handling_invalid_lua() {
        let steps = vec![ScriptStep::Map("invalid lua code here!!!".to_string())];
        let result = LuaRunner::new(steps);
        assert!(result.is_err());
    }

    #[test]
    fn test_runtime_error_in_filter() {
        let steps = vec![ScriptStep::Filter(
            "return doc.nonexistent.field > 10".to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"value": 42});
        let result = runner.run(input);
        assert!(result.is_err());
    }

    #[test]
    fn test_runtime_error_in_map() {
        let steps = vec![ScriptStep::Map(
            "return doc.nonexistent.field * 2".to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"value": 42});
        let result = runner.run(input);
        assert!(result.is_err());
    }

    #[test]
    fn test_modification_of_shared_value() {
        let steps = vec![ScriptStep::Map(
            r#"
            doc.new_field = "added"
            doc.nested = doc.nested or {}
            doc.nested.count = (doc.nested.count or 0) + 1
            return doc
        "#
            .to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"existing": "value"});
        let result = runner.run(input).unwrap();
        let expected = json!({
            "existing": "value",
            "new_field": "added",
            "nested": {"count": 1}
        });
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_pairs_iteration_on_object() {
        let steps = vec![ScriptStep::Map(
            r#"
            local keys = {}
            for k, v in pairs(doc) do
                keys[#keys + 1] = k
            end
            table.sort(keys)
            doc._keys = keys
            return doc
        "#
            .to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"b": 2, "a": 1, "c": 3});
        let result = runner.run(input).unwrap();
        let expected = json!({
            "a": 1, "b": 2, "c": 3,
            "_keys": ["a", "b", "c"]
        });
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_chain_modifications() {
        let steps = vec![
            ScriptStep::Map("doc.step1 = true; return doc".to_string()),
            ScriptStep::Filter("return doc.step1 == true".to_string()),
            ScriptStep::Map("doc.step2 = doc.step1 and 'yes' or 'no'; return doc".to_string()),
        ];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"initial": "data"});
        let result = runner.run(input).unwrap();
        let expected = json!({
            "initial": "data",
            "step1": true,
            "step2": "yes"
        });
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_key_deletion() {
        let steps = vec![ScriptStep::Map(
            r#"
            doc.to_keep = "keep this"
            doc.to_delete = "delete this"
            doc.nested = {keep = "nested keep", delete = "nested delete"}
            
            doc.to_delete = nil
            doc.nested.delete = nil
            
            return doc
        "#
            .to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!({"existing": "value"});
        let result = runner.run(input).unwrap();
        let expected = json!({
            "existing": "value",
            "to_keep": "keep this",
            "nested": {"keep": "nested keep"}
        });
        assert_eq!(result, Some(expected));
    }

    #[test]
    fn test_remove_array_element() {
        let steps = vec![ScriptStep::Map(
            r#"
        doc[2] = nil
        return doc
        "#
            .to_string(),
        )];
        let runner = LuaRunner::new(steps).unwrap();
        let input = json!([1, 2, 3, 4]);
        let result = runner.run(input).unwrap();
        let expected = json!([1, null, 3, 4]);
        assert_eq!(result, Some(expected));
    }
}
