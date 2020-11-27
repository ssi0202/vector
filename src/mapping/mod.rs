use crate::event::{Event, LookupBuf, Value};
use std::collections::BTreeMap;
use std::convert::TryFrom;

pub mod parser;
pub mod query;

use query::query_value::QueryValue;

pub type Result<T> = std::result::Result<T, String>;

pub(self) trait Function: Send + core::fmt::Debug {
    fn apply(&self, target: &mut Event) -> Result<()>;
}

//------------------------------------------------------------------------------

#[derive(Debug)]
pub(self) struct Assignment {
    path: LookupBuf,
    function: Box<dyn query::Function>,
}

impl Assignment {
    pub(self) fn new(path: LookupBuf, function: Box<dyn query::Function>) -> Self {
        Self { path, function }
    }
}

impl Function for Assignment {
    fn apply(&self, target: &mut Event) -> Result<()> {
        match self.function.execute(&target)? {
            QueryValue::Value(v) => {
                target.as_mut_log().insert(self.path.clone(), v);
                Ok(())
            }
            _ => Err("assignment must be from a value".to_string()),
        }
    }
}

//------------------------------------------------------------------------------

#[derive(Debug)]
pub(self) struct Deletion {
    paths: Vec<LookupBuf>,
}

impl Deletion {
    pub(self) fn new(mut paths: Vec<LookupBuf>) -> Self {
        Self {
            paths: paths.drain(..).collect(),
        }
    }
}

impl Function for Deletion {
    fn apply(&self, target: &mut Event) -> Result<()> {
        for path in &self.paths {
            target.as_mut_log().remove(path, false);
        }
        Ok(())
    }
}

//------------------------------------------------------------------------------

#[derive(Debug)]
pub(self) struct OnlyFields {
    paths: Vec<LookupBuf>,
}

impl OnlyFields {
    pub(self) fn new(paths: Vec<LookupBuf>) -> Self {
        Self { paths }
    }
}

impl Function for OnlyFields {
    fn apply(&self, target: &mut Event) -> Result<()> {
        let target_log = target.as_mut_log();

        let keys: Vec<LookupBuf> = target_log
            .keys(true)
            .filter(|k| self.paths.iter().find(|&p| k == &p.into()).is_none())
            // Shed borrow so we can remove these keys.
            .map(|v| v.into_buf())
            .collect();

        for key in keys {
            target_log.remove(&key, true);
        }

        Ok(())
    }
}

//------------------------------------------------------------------------------

#[derive(Debug)]
pub(self) struct IfStatement {
    query: Box<dyn query::Function>,
    true_statement: Box<dyn Function>,
    false_statement: Box<dyn Function>,
}

impl IfStatement {
    pub(self) fn new(
        query: Box<dyn query::Function>,
        true_statement: Box<dyn Function>,
        false_statement: Box<dyn Function>,
    ) -> Self {
        Self {
            query,
            true_statement,
            false_statement,
        }
    }
}

impl Function for IfStatement {
    fn apply(&self, target: &mut Event) -> Result<()> {
        match self.query.execute(target)? {
            QueryValue::Value(Value::Boolean(true)) => self.true_statement.apply(target),
            QueryValue::Value(Value::Boolean(false)) => self.false_statement.apply(target),
            _ => Err("query returned non-boolean value".to_string()),
        }
    }
}

//------------------------------------------------------------------------------

#[derive(Debug)]
pub(self) struct Noop {}

impl Function for Noop {
    fn apply(&self, _: &mut Event) -> Result<()> {
        Ok(())
    }
}

//------------------------------------------------------------------------------

#[derive(Debug)]
pub struct Mapping {
    assignments: Vec<Box<dyn Function>>,
}

impl Mapping {
    pub(self) fn new(assignments: Vec<Box<dyn Function>>) -> Self {
        Mapping { assignments }
    }

    pub fn execute(&self, event: &mut Event) -> Result<()> {
        for (i, assignment) in self.assignments.iter().enumerate() {
            if let Err(err) = assignment.apply(event) {
                return Err(format!("failed to apply mapping {}: {}", i, err));
            }
        }
        Ok(())
    }
}

//------------------------------------------------------------------------------

/// Merges two BTreeMaps of `Value`s.
/// The second map is merged into the first one.
///
/// If `deep` is true, only the top level values are merged in. If both maps contain a field
/// with the same name, the field from the first is overwritten with the field from the second.
///
/// If `deep` is false, should both maps contain a field with the same name, and both those
/// fields are also maps, the function will recurse and will merge the child fields from the second
/// into the child fields from the first.
///
/// Note, this does recurse, so there is the theoretical possibility that it could blow up the
/// stack. From quick tests on a sample project I was able to merge maps with a depth of 3,500
/// before encountering issues. So I think that is likely to be within acceptable limits.
/// If it becomes a problem, we can unroll this function, but that will come at a cost of extra
/// code complexity.
fn merge_maps<K>(map1: &mut BTreeMap<K, Value>, map2: &BTreeMap<K, Value>, deep: bool)
where
    K: std::cmp::Ord + Clone,
{
    for (key2, value2) in map2.iter() {
        match (deep, map1.get_mut(key2), value2) {
            (true, Some(Value::Map(ref mut child1)), Value::Map(ref child2)) => {
                // We are doing a deep merge and both fields are maps.
                merge_maps(child1, child2, deep);
            }
            _ => {
                map1.insert(key2.clone(), value2.clone());
            }
        }
    }
}

#[derive(Debug)]
pub(in crate::mapping) struct MergeFn {
    to_path: LookupBuf,
    from: Box<dyn query::Function>,
    deep: Option<Box<dyn query::Function>>,
}

impl MergeFn {
    pub(in crate::mapping) fn new(
        to_path: LookupBuf,
        from: Box<dyn query::Function>,
        deep: Option<Box<dyn query::Function>>,
    ) -> Self {
        MergeFn {
            to_path,
            from,
            deep,
        }
    }
}

impl Function for MergeFn {
    fn apply(&self, target: &mut Event) -> Result<()> {
        let from_value = self.from.execute(target)?;
        let deep = match &self.deep {
            None => false,
            Some(deep) => match deep.execute(target)? {
                QueryValue::Value(Value::Boolean(value)) => value,
                _ => return Err("deep parameter passed to merge is a non-boolean value".into()),
            },
        };

        let to_value = target.as_mut_log().get_mut(&self.to_path).ok_or(format!(
            "parameter {} passed to merge is not found",
            self.to_path
        ))?;

        match (to_value, from_value) {
            (Value::Map(ref mut map1), QueryValue::Value(Value::Map(ref map2))) => {
                merge_maps(map1, &map2, deep);
                Ok(())
            }

            _ => Err("parameters passed to merge are non-map values".into()),
        }
    }
}

//------------------------------------------------------------------------------

/// Represents the different log levels that can be used by LogFn
#[derive(Debug, Clone, Copy)]
pub(in crate::mapping) enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl TryFrom<&str> for LogLevel {
    type Error = String;

    fn try_from(level: &str) -> Result<Self> {
        match level {
            "trace" => Ok(Self::Trace),
            "debug" => Ok(Self::Debug),
            "info" => Ok(Self::Info),
            "warn" => Ok(Self::Warn),
            "error" => Ok(Self::Error),
            _ => Err("invalid log level".to_string()),
        }
    }
}

#[derive(Debug)]
pub(in crate::mapping) struct LogFn {
    msg: Box<dyn query::Function>,
    level: Option<LogLevel>,
}

impl LogFn {
    pub(in crate::mapping) fn new(msg: Box<dyn query::Function>, level: Option<LogLevel>) -> Self {
        Self { msg, level }
    }
}

impl Function for LogFn {
    fn apply(&self, target: &mut Event) -> Result<()> {
        let msg = match self.msg.execute(target)? {
            QueryValue::Value(value) => value,
            _ => return Err("Can only log Value parameters".to_string()),
        };
        let msg = msg.clone_into_bytes();
        let string = String::from_utf8_lossy(&msg);
        let level = self.level.unwrap_or(LogLevel::Info);

        match level {
            LogLevel::Trace => trace!("{}", string),
            LogLevel::Debug => debug!("{}", string),
            LogLevel::Info => info!("{}", string),
            LogLevel::Warn => warn!("{}", string),
            LogLevel::Error => error!("{}", string),
        }

        Ok(())
    }
}

//------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::query::{
        arithmetic::Arithmetic, arithmetic::Operator as ArithmeticOperator,
        path::Path as QueryPath, Literal,
    };
    use super::*;
    use crate::event::{Event, Lookup, Value};

    #[test]
    fn check_mapping() {
        crate::test_util::trace_init();

        let cases = vec![
            (
                {
                    let mut event = Event::from("foo body");
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                {
                    let mut event = Event::from("foo body");
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("foo"), Value::from("bar"));
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                Mapping::new(vec![Box::new(Assignment::new(
                    LookupBuf::from("foo"),
                    Box::new(Literal::from(Value::from("bar"))),
                ))]),
                Ok(()),
            ),
            (
                {
                    let mut event = Event::from("foo body");
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                {
                    let mut event = Event::from("foo body");
                    event.as_mut_log().insert(
                        LookupBuf::from_str("foo bar\\.baz.buz").unwrap(),
                        Value::from("quack"),
                    );
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                Mapping::new(vec![Box::new(Assignment::new(
                    LookupBuf::from_str("foo bar\\.baz.buz").unwrap(),
                    Box::new(Literal::from(Value::from("quack"))),
                ))]),
                Ok(()),
            ),
            (
                {
                    let mut event = Event::from("foo body");
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("foo"), Value::from("bar"));
                    event
                },
                {
                    let mut event = Event::from("foo body");
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                Mapping::new(vec![Box::new(Deletion::new(vec![LookupBuf::from("foo")]))]),
                Ok(()),
            ),
            (
                {
                    let mut event = Event::from("foo body");
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("bar"), Value::from("baz"));
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                {
                    let mut event = Event::from("foo body");
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("foo"), Value::from("bar"));
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                Mapping::new(vec![
                    Box::new(Assignment::new(
                        LookupBuf::from("foo"),
                        Box::new(Literal::from(Value::from("bar"))),
                    )),
                    Box::new(Deletion::new(vec![LookupBuf::from("bar")])),
                ]),
                Ok(()),
            ),
            (
                {
                    let mut event = Event::from("foo body");
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("bar"), Value::from("baz"));
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                {
                    let mut event = Event::from("foo body");
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("bar"), Value::from("baz"));
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("foo"), Value::from("bar is baz"));
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                Mapping::new(vec![Box::new(IfStatement::new(
                    Box::new(Arithmetic::new(
                        Box::new(QueryPath::from("bar")),
                        Box::new(Literal::from(Value::from("baz"))),
                        ArithmeticOperator::Equal,
                    )),
                    Box::new(Assignment::new(
                        LookupBuf::from("foo"),
                        Box::new(Literal::from(Value::from("bar is baz"))),
                    )),
                    Box::new(Deletion::new(vec![LookupBuf::from("bar")])),
                ))]),
                Ok(()),
            ),
            (
                {
                    let mut event = Event::from("foo body");
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("bar"), Value::from("buz"));
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                {
                    let mut event = Event::from("foo body");
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                Mapping::new(vec![Box::new(IfStatement::new(
                    Box::new(Arithmetic::new(
                        Box::new(QueryPath::from("bar")),
                        Box::new(Literal::from(Value::from("baz"))),
                        ArithmeticOperator::Equal,
                    )),
                    Box::new(Assignment::new(
                        LookupBuf::from("foo"),
                        Box::new(Literal::from(Value::from("bar is baz"))),
                    )),
                    Box::new(Deletion::new(vec![LookupBuf::from("bar")])),
                ))]),
                Ok(()),
            ),
            (
                {
                    let mut event = Event::from("foo body");
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("bar"), Value::from("buz"));
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                {
                    let mut event = Event::from("foo body");
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("bar"), Value::from("buz"));
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                Mapping::new(vec![Box::new(IfStatement::new(
                    Box::new(QueryPath::from("bar")),
                    Box::new(Assignment::new(
                        LookupBuf::from("foo"),
                        Box::new(Literal::from(Value::from("bar is baz"))),
                    )),
                    Box::new(Deletion::new(vec![LookupBuf::from("bar")])),
                ))]),
                Err("failed to apply mapping 0: query returned non-boolean value".to_string()),
            ),
            (
                {
                    let mut event = Event::from("foo body");
                    event.as_mut_log().insert(
                        LookupBuf::from_str("bar.baz.buz").unwrap(),
                        Value::from("first"),
                    );
                    event.as_mut_log().insert(
                        LookupBuf::from_str("bar.baz.remove_this").unwrap(),
                        Value::from("second"),
                    );
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from_str("bev").unwrap(), Value::from("third"));
                    event.as_mut_log().insert(
                        LookupBuf::from_str("and.remove_this").unwrap(),
                        Value::from("fourth"),
                    );
                    event.as_mut_log().insert(
                        LookupBuf::from_str("nested.stuff.here").unwrap(),
                        Value::from("fifth"),
                    );
                    event.as_mut_log().insert(
                        LookupBuf::from_str("nested.and_here").unwrap(),
                        Value::from("sixth"),
                    );
                    event
                },
                {
                    let mut event = Event::from("foo body");
                    event.as_mut_log().insert(
                        LookupBuf::from_str("bar.baz.buz").unwrap(),
                        Value::from("first"),
                    );
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from_str("bev").unwrap(), Value::from("third"));
                    event.as_mut_log().insert(
                        LookupBuf::from_str("nested.stuff.here").unwrap(),
                        Value::from("fifth"),
                    );
                    event.as_mut_log().insert(
                        LookupBuf::from_str("nested.and_here").unwrap(),
                        Value::from("sixth"),
                    );
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event.as_mut_log().remove(Lookup::from("message"), false);
                    event
                },
                Mapping::new(vec![Box::new(OnlyFields::new(vec![
                    LookupBuf::from_str("bar.baz.buz").unwrap(),
                    LookupBuf::from_str("bev").unwrap(),
                    LookupBuf::from_str("doesnt_exist.anyway").unwrap(),
                    LookupBuf::from_str("nested").unwrap(),
                ]))]),
                Ok(()),
            ),
        ];

        for (mut input_event, exp_event, mapping, exp_result) in cases {
            assert_eq!(mapping.execute(&mut input_event), exp_result);
            assert_eq!(input_event, exp_event);
        }
    }

    #[test]
    fn check_merge() {
        let cases = vec![
            (
                {
                    let mut event = Event::from("");
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("foo"), Value::Boolean(true));
                    event.as_mut_log().insert(
                        LookupBuf::from("bar"),
                        serde_json::json!({ "key2": "val2" }),
                    );
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                {
                    let mut event = Event::from("");
                    event
                        .as_mut_log()
                        .insert(LookupBuf::from("foo"), Value::Boolean(true));
                    event.as_mut_log().insert(
                        LookupBuf::from("bar"),
                        serde_json::json!({ "key2": "val2" }),
                    );
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event
                },
                Mapping::new(vec![Box::new(MergeFn::new(
                    "foo".into(),
                    Box::new(QueryPath::from(vec![vec!["bar"]])),
                    None,
                ))]),
                Err(
                    "failed to apply mapping 0: parameters passed to merge are non-map values"
                        .into(),
                ),
            ),
            (
                {
                    let mut event = Event::from("");
                    event.as_mut_log().insert(
                        LookupBuf::from("foo"),
                        serde_json::json!({ "key1": "val1" }),
                    );
                    event.as_mut_log().insert(
                        LookupBuf::from("bar"),
                        serde_json::json!({ "key2": "val2" }),
                    );
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event.as_mut_log().remove(Lookup::from("message"), false);
                    event
                },
                {
                    let mut event = Event::from("");
                    event.as_mut_log().insert(
                        LookupBuf::from("foo"),
                        serde_json::json!({ "key1": "val1", "key2": "val2" }),
                    );
                    event.as_mut_log().insert(
                        LookupBuf::from("bar"),
                        serde_json::json!({ "key2": "val2" }),
                    );
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event.as_mut_log().remove(Lookup::from("message"), false);
                    event
                },
                Mapping::new(vec![Box::new(MergeFn::new(
                    "foo".into(),
                    Box::new(QueryPath::from(vec![vec!["bar"]])),
                    None,
                ))]),
                Ok(()),
            ),
            (
                {
                    let mut event = Event::from("");
                    event.as_mut_log().insert(
                        LookupBuf::from("parent1"),
                        serde_json::json!(
                        { "key1": "val1",
                          "child": {
                              "grandchild1": "val1"
                          }
                        }),
                    );
                    event.as_mut_log().insert(
                        LookupBuf::from("parent2"),
                        serde_json::json!(
                            { "key2": "val2",
                               "child": {
                                   "grandchild2": "val2"
                               }
                        }),
                    );
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event.as_mut_log().remove(Lookup::from("message"), false);
                    event
                },
                {
                    let mut event = Event::from("");
                    event.as_mut_log().insert(
                        LookupBuf::from("parent1"),
                        serde_json::json!(
                        { "key1": "val1",
                          "key2": "val2",
                          "child": {
                             "grandchild2": "val2"
                          }
                        }),
                    );
                    event.as_mut_log().insert(
                        LookupBuf::from("parent2"),
                        serde_json::json!(
                            { "key2": "val2",
                              "child": {
                                  "grandchild2": "val2"
                              }
                        }),
                    );
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event.as_mut_log().remove(Lookup::from("message"), false);
                    event
                },
                Mapping::new(vec![Box::new(MergeFn::new(
                    "parent1".into(),
                    Box::new(QueryPath::from(vec![vec!["parent2"]])),
                    None,
                ))]),
                Ok(()),
            ),
            (
                {
                    let mut event = Event::from("");
                    event.as_mut_log().insert(
                        LookupBuf::from("parent1"),
                        serde_json::json!(
                        { "key1": "val1",
                          "child": {
                              "grandchild1": "val1"
                          }
                        }),
                    );
                    event.as_mut_log().insert(
                        LookupBuf::from("parent2"),
                        serde_json::json!(
                            { "key2": "val2",
                               "child": {
                                   "grandchild2": "val2"
                               }
                        }),
                    );
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event.as_mut_log().remove(Lookup::from("message"), false);
                    event
                },
                {
                    let mut event = Event::from("");
                    event.as_mut_log().insert(
                        LookupBuf::from("parent1"),
                        serde_json::json!(
                        { "key1": "val1",
                          "key2": "val2",
                          "child": {
                              "grandchild1": "val1",
                              "grandchild2": "val2"
                          }
                        }),
                    );
                    event.as_mut_log().insert(
                        LookupBuf::from("parent2"),
                        serde_json::json!(
                            { "key2": "val2",
                              "child": {
                                  "grandchild2": "val2"
                              }
                        }),
                    );
                    event.as_mut_log().remove(Lookup::from("timestamp"), false);
                    event.as_mut_log().remove(Lookup::from("message"), false);
                    event
                },
                Mapping::new(vec![Box::new(MergeFn::new(
                    "parent1".into(),
                    Box::new(QueryPath::from(vec![vec!["parent2"]])),
                    Some(Box::new(Literal::from(Value::Boolean(true)))),
                ))]),
                Ok(()),
            ),
        ];

        for (mut input_event, exp_event, mapping, exp_result) in cases {
            assert_eq!(mapping.execute(&mut input_event), exp_result);
            assert_eq!(input_event, exp_event);
        }
    }
}
