use crate::Limits;
use crate::json::{encode, to_json};
use cel::{Context, ExecutionError, FunctionContext, Value};
use std::sync::Arc;

fn failure() -> ExecutionError {
    ExecutionError::function_error("workflow", "EXPRESSION_EVALUATION")
}
fn string(v: &Value) -> Result<&str, ExecutionError> {
    if let Value::String(s) = v {
        Ok(s)
    } else {
        Err(failure())
    }
}
fn index(v: &Value) -> Result<usize, ExecutionError> {
    if let Value::Int(i) = v {
        usize::try_from(*i).map_err(|_| failure())
    } else {
        Err(failure())
    }
}
fn integer_syntax(s: &str, signed: bool) -> bool {
    let s = if signed {
        s.strip_prefix('-').unwrap_or(s)
    } else {
        s
    };
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}
fn checked_int(v: &Value) -> Result<Value, ExecutionError> {
    Ok(Value::Int(match v {
        Value::Int(i) => *i,
        Value::UInt(i) => i64::try_from(*i).map_err(|_| failure())?,
        Value::Float(f)
            if f.is_finite()
                && *f >= -9_223_372_036_854_775_808.0
                && *f < 9_223_372_036_854_775_808.0 =>
        {
            f.trunc() as i64
        }
        Value::String(s) if integer_syntax(s, true) => s.parse().map_err(|_| failure())?,
        _ => return Err(failure()),
    }))
}
fn checked_uint(v: &Value) -> Result<Value, ExecutionError> {
    Ok(Value::UInt(match v {
        Value::UInt(i) => *i,
        Value::Int(i) => u64::try_from(*i).map_err(|_| failure())?,
        Value::Float(f) if f.is_finite() && *f >= 0.0 && *f < 18_446_744_073_709_551_616.0 => {
            f.trunc() as u64
        }
        Value::String(s) if integer_syntax(s, false) => s.parse().map_err(|_| failure())?,
        _ => return Err(failure()),
    }))
}
fn checked_double(v: &Value) -> Result<Value, ExecutionError> {
    let f = match v {
        Value::Int(i) => *i as f64,
        Value::UInt(i) => *i as f64,
        Value::Float(f) => *f,
        Value::String(s) => {
            // serde_json validates the full JSON-number grammar and float_roundtrip is unified by the workspace.
            if s.trim() != s.as_str() {
                return Err(failure());
            }
            let n: serde_json::Number = serde_json::from_str(s).map_err(|_| failure())?;
            n.as_f64().ok_or_else(failure)?
        }
        _ => return Err(failure()),
    };
    if !f.is_finite() {
        return Err(failure());
    }
    Ok(Value::Float(f))
}
fn dispatch(name: &str, args: &[Value], limits: Limits) -> Result<Value, ExecutionError> {
    let v = args.first().ok_or_else(failure)?;
    let unary = matches!(
        name,
        "size" | "int" | "uint" | "double" | "string" | "bytes" | "jsonEncode"
    );
    if unary && args.len() != 1 {
        return Err(failure());
    }
    Ok(match name {
        "int" => checked_int(v)?,
        "uint" => checked_uint(v)?,
        "double" => checked_double(v)?,
        "size" => Value::Int(
            i64::try_from(match v {
                Value::String(s) => s.chars().count(),
                Value::Bytes(b) => b.len(),
                Value::List(l) => l.len(),
                Value::Map(m) => m.map.len(),
                _ => return Err(failure()),
            })
            .map_err(|_| failure())?,
        ),
        "string" => Value::String(Arc::new(match v {
            Value::String(s) => (**s).clone(),
            Value::Int(i) => i.to_string(),
            Value::UInt(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            _ => return Err(failure()),
        })),
        "bytes" => match v {
            Value::String(s) => Value::Bytes(Arc::new(s.as_bytes().to_vec())),
            Value::Bytes(b) => Value::Bytes(b.clone()),
            _ => return Err(failure()),
        },
        "contains" | "startsWith" | "endsWith" => {
            if args.len() != 2 {
                return Err(failure());
            }
            let found = match (name, v, &args[1]) {
                ("contains", Value::List(l), needle) => l.contains(needle),
                ("contains", Value::Map(m), needle) => {
                    let key = match needle {
                        Value::String(v) => cel::objects::Key::String(v.clone()),
                        Value::Int(v) => cel::objects::Key::Int(*v),
                        Value::UInt(v) => cel::objects::Key::Uint(*v),
                        Value::Bool(v) => cel::objects::Key::Bool(*v),
                        _ => return Err(failure()),
                    };
                    m.map.contains_key(&key)
                }
                (_, Value::String(s), Value::String(n)) => match name {
                    "contains" => s.contains(n.as_str()),
                    "startsWith" => s.starts_with(n.as_str()),
                    _ => s.ends_with(n.as_str()),
                },
                _ => return Err(failure()),
            };
            Value::Bool(found)
        }
        "split" => {
            if !(2..=3).contains(&args.len()) {
                return Err(failure());
            }
            let s = string(v)?;
            let sep = string(&args[1])?;
            let count = match args.get(2) {
                None => usize::MAX,
                Some(Value::Int(i)) if *i < 0 => usize::MAX,
                Some(v) => index(v)?,
            };
            let mut out = Vec::new();
            let mut add = |s: &str| -> Result<(), ExecutionError> {
                if out.len() >= limits.output_nodes.saturating_sub(1) {
                    return Err(ExecutionError::function_error(
                        "workflow",
                        "EXPRESSION_LIMIT",
                    ));
                }
                out.push(Value::String(Arc::new(s.to_owned())));
                Ok(())
            };
            if count > 0 {
                if sep.is_empty() {
                    for (n, (i, ch)) in s.char_indices().enumerate() {
                        if n + 1 == count {
                            add(&s[i..])?;
                            break;
                        }
                        add(&s[i..i + ch.len_utf8()])?;
                    }
                } else {
                    for part in s.splitn(count, sep) {
                        add(part)?;
                    }
                }
            }
            Value::List(Arc::new(out))
        }
        "substring" => {
            if !(2..=3).contains(&args.len()) {
                return Err(failure());
            }
            let s = string(v)?;
            let start = index(&args[1])?;
            let end = args
                .get(2)
                .map(index)
                .transpose()?
                .unwrap_or_else(|| s.chars().count());
            if start > end {
                return Err(failure());
            }
            let offsets: Vec<_> = s
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(s.len()))
                .collect();
            let (Some(start), Some(end)) = (offsets.get(start), offsets.get(end)) else {
                return Err(failure());
            };
            Value::String(Arc::new(s[*start..*end].to_owned()))
        }
        "join" => {
            if !(1..=2).contains(&args.len()) {
                return Err(failure());
            }
            let Value::List(list) = v else {
                return Err(failure());
            };
            let sep = args.get(1).map(string).transpose()?.unwrap_or("");
            let mut bytes = sep.len().saturating_mul(list.len().saturating_sub(1));
            for v in list.iter() {
                bytes = bytes.saturating_add(string(v)?.len());
            }
            if bytes > limits.output_bytes {
                return Err(ExecutionError::function_error(
                    "workflow",
                    "EXPRESSION_LIMIT",
                ));
            }
            let mut out = String::with_capacity(bytes);
            for (i, v) in list.iter().enumerate() {
                if i != 0 {
                    out.push_str(sep);
                }
                out.push_str(string(v)?);
            }
            Value::String(Arc::new(out))
        }
        "indexOf" => {
            if !(2..=3).contains(&args.len()) {
                return Err(failure());
            }
            let s = string(v)?;
            let needle = string(&args[1])?;
            let start = args.get(2).map(index).transpose()?.unwrap_or(0);
            let byte = s
                .char_indices()
                .map(|(i, _)| i)
                .chain(std::iter::once(s.len()))
                .nth(start)
                .ok_or_else(failure)?;
            Value::Int(
                s[byte..]
                    .find(needle)
                    .map_or(-1, |i| s[..byte + i].chars().count() as i64),
            )
        }
        "jsonEncode" => {
            let json = to_json(v, limits, 0, &mut 0)
                .map_err(|e| ExecutionError::function_error("workflow", e.category.code()))?;
            let bytes = encode(&json, limits.output_bytes)
                .map_err(|e| ExecutionError::function_error("workflow", e.category.code()))?;
            Value::String(Arc::new(String::from_utf8(bytes).map_err(|_| failure())?))
        }
        _ => return Err(failure()),
    })
}
/// Empty Env means no native overload can precede these explicit registrations.
pub(crate) fn context(limits: Limits) -> Context<'static> {
    let mut ctx = Context::empty();
    for name in [
        "size",
        "contains",
        "startsWith",
        "endsWith",
        "int",
        "uint",
        "double",
        "string",
        "bytes",
        "split",
        "substring",
        "join",
        "indexOf",
        "jsonEncode",
    ] {
        ctx.add_function(
            name,
            move |ftx: &FunctionContext| -> Result<Value, ExecutionError> {
                let args = ftx
                    .this
                    .iter()
                    .chain(ftx.args.iter())
                    .map(|v| Value::try_from(v.as_ref()))
                    .collect::<Result<Vec<_>, _>>()?;
                dispatch(name, &args, limits)
            },
        );
    }
    ctx
}
