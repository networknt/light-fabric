use super::*;
struct Parser(Box<unsafe_libyaml::yaml_parser_t>);
impl Drop for Parser {
    fn drop(&mut self) {
        unsafe { unsafe_libyaml::yaml_parser_delete(&mut *self.0) }
    }
}
struct Event(unsafe_libyaml::yaml_event_t);
impl Drop for Event {
    fn drop(&mut self) {
        unsafe { unsafe_libyaml::yaml_event_delete(&mut self.0) }
    }
}
unsafe fn name(pointer: *const u8) -> io::Result<String> {
    if pointer.is_null() {
        return Ok(String::new());
    }
    let bytes = unsafe { CStr::from_ptr(pointer.cast()).to_bytes() };
    if bytes.len() > RAW {
        return Err(bad());
    }
    Ok(std::str::from_utf8(bytes).map_err(|_| bad())?.into())
}
struct Frame {
    map: bool,
    key: Option<String>,
    value: Value,
    anchor: String,
}
fn count(value: &Value, depth: usize, total: &mut usize, nodes: &mut usize) -> io::Result<()> {
    if depth > 64 {
        return Err(bad());
    }
    *nodes += 1;
    if *nodes > 65536 {
        return Err(bad());
    }
    let add = match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(n) => n.as_str().len(),
        Value::String(s) => quoted(s)?,
        Value::Array(a) => {
            for v in a {
                count(v, depth + 1, total, nodes)?
            }
            2 + a.len().saturating_sub(1)
        }
        Value::Object(m) => {
            for (k, v) in m {
                let bytes = quoted(k)? + 1;
                *total = total.checked_add(bytes).ok_or_else(bad)?;
                if *total > EXPANDED {
                    return Err(bad());
                }
                count(v, depth + 1, total, nodes)?
            }
            2 + m.len().saturating_sub(1)
        }
    };
    *total = total.checked_add(add).ok_or_else(bad)?;
    if *total > EXPANDED {
        return Err(bad());
    }
    Ok(())
}
fn quoted(text: &str) -> io::Result<usize> {
    let mut n = 2usize;
    for c in text.chars() {
        n = n
            .checked_add(match c {
                '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
                c if (c as u32) < 32 => 6,
                c => c.len_utf8(),
            })
            .ok_or_else(bad)?;
        if n > EXPANDED {
            return Err(bad());
        }
    }
    Ok(n)
}
fn scalar(text: &str, plain: bool, tag: &str) -> io::Result<Value> {
    if !plain || tag == "tag:yaml.org,2002:str" {
        return Ok(Value::String(text.into()));
    }
    if let Ok(number) = serde_json::Number::from_str(text) {
        return Ok(Value::Number(number));
    }
    // Existing scalar semantics are retained where exact JSON lexemes do not apply.
    let scalar: serde_yaml::Value = serde_yaml::from_str(text).map_err(|_| bad())?;
    serde_json::to_value(scalar).map_err(|_| bad())
}
fn yaml_document(raw: &[u8]) -> io::Result<Value> {
    let mut init = Box::new(MaybeUninit::<unsafe_libyaml::yaml_parser_t>::uninit());
    if unsafe { unsafe_libyaml::yaml_parser_initialize(init.as_mut_ptr()) }.fail {
        return Err(bad());
    }
    let mut parser = Parser(unsafe { init.assume_init() });
    unsafe {
        unsafe_libyaml::yaml_parser_set_input_string(&mut *parser.0, raw.as_ptr(), raw.len() as u64)
    };
    let mut stack: Vec<Frame> = Vec::new();
    let mut anchors = BTreeMap::<String, Value>::new();
    let mut root = None;
    let mut documents = 0;
    let mut expanded_bytes = 0usize;
    let mut expanded_nodes = 0usize;
    let mut anchor_bytes = 0usize;
    for _ in 0..65536 {
        let mut init = MaybeUninit::<unsafe_libyaml::yaml_event_t>::uninit();
        if unsafe { unsafe_libyaml::yaml_parser_parse(&mut *parser.0, init.as_mut_ptr()) }.fail {
            return Err(bad());
        }
        let event = Event(unsafe { init.assume_init() });
        let (value, anchor) = unsafe {
            match event.0.type_ {
                unsafe_libyaml::YAML_STREAM_START_EVENT
                | unsafe_libyaml::YAML_DOCUMENT_END_EVENT => continue,
                unsafe_libyaml::YAML_DOCUMENT_START_EVENT => {
                    documents += 1;
                    if documents > 1 {
                        return Err(bad());
                    }
                    continue;
                }
                unsafe_libyaml::YAML_STREAM_END_EVENT => {
                    return if stack.is_empty() {
                        root.ok_or_else(bad)
                    } else {
                        Err(bad())
                    };
                }
                unsafe_libyaml::YAML_MAPPING_START_EVENT
                | unsafe_libyaml::YAML_SEQUENCE_START_EVENT => {
                    if stack.len() >= 64 {
                        return Err(bad());
                    }
                    let map = event.0.type_ == unsafe_libyaml::YAML_MAPPING_START_EVENT;
                    let anchor = if map {
                        name(event.0.data.mapping_start.anchor)?
                    } else {
                        name(event.0.data.sequence_start.anchor)?
                    };
                    stack.push(Frame {
                        map,
                        key: None,
                        value: if map {
                            Value::Object(Map::new())
                        } else {
                            Value::Array(Vec::new())
                        },
                        anchor,
                    });
                    continue;
                }
                unsafe_libyaml::YAML_MAPPING_END_EVENT
                | unsafe_libyaml::YAML_SEQUENCE_END_EVENT => {
                    let f = stack.pop().ok_or_else(bad)?;
                    if f.key.is_some() {
                        return Err(bad());
                    }
                    (f.value, f.anchor)
                }
                unsafe_libyaml::YAML_ALIAS_EVENT => {
                    let key = name(event.0.data.alias.anchor)?;
                    let value = anchors.get(&key).ok_or_else(bad)?;
                    let mut bytes = 0;
                    let mut nodes = 0;
                    count(value, stack.len(), &mut bytes, &mut nodes)?;
                    if expanded_bytes.checked_add(bytes).ok_or_else(bad)? > EXPANDED
                        || expanded_nodes.checked_add(nodes).ok_or_else(bad)? > 65536
                    {
                        return Err(bad());
                    }
                    expanded_bytes += bytes;
                    expanded_nodes += nodes;
                    (value.clone(), String::new())
                }
                unsafe_libyaml::YAML_SCALAR_EVENT => {
                    let s = event.0.data.scalar;
                    if s.length as usize > RAW {
                        return Err(bad());
                    }
                    let text =
                        std::str::from_utf8(std::slice::from_raw_parts(s.value, s.length as usize))
                            .map_err(|_| bad())?;
                    let tag = name(s.tag)?;
                    let key = stack.last().is_some_and(|f| f.map && f.key.is_none());
                    let value = if key {
                        Value::String(text.into())
                    } else {
                        scalar(
                            text,
                            s.style == unsafe_libyaml::YAML_PLAIN_SCALAR_STYLE,
                            &tag,
                        )?
                    };
                    expanded_bytes = expanded_bytes.checked_add(text.len()).ok_or_else(bad)?;
                    expanded_nodes += 1;
                    if expanded_bytes > EXPANDED || expanded_nodes > 65536 {
                        return Err(bad());
                    }
                    (value, name(s.anchor)?)
                }
                _ => return Err(bad()),
            }
        };
        if !anchor.is_empty() {
            let mut bytes = 0;
            let mut nodes = 0;
            count(&value, 0, &mut bytes, &mut nodes)?;
            anchor_bytes = anchor_bytes.checked_add(bytes).ok_or_else(bad)?;
            if anchor_bytes > EXPANDED {
                return Err(bad());
            }
            anchors.insert(anchor, value.clone());
        }
        if let Some(frame) = stack.last_mut() {
            if frame.map {
                if let Some(key) = frame.key.take() {
                    frame.value.as_object_mut().unwrap().insert(key, value);
                } else {
                    frame.key = Some(value.as_str().ok_or_else(bad)?.into())
                }
            } else {
                frame.value.as_array_mut().unwrap().push(value)
            }
        } else if root.replace(value).is_some() {
            return Err(bad());
        }
    }
    Err(bad())
}

pub(super) fn parse(raw: &[u8], yaml: bool) -> io::Result<Value> {
    if raw.len() > RAW {
        return Err(bad());
    }
    let value = if yaml {
        yaml_document(raw)?
    } else {
        serde_json::from_slice(raw).map_err(|_| bad())?
    };
    let mut bytes = 0;
    let mut nodes = 0;
    count(&value, 0, &mut bytes, &mut nodes)?;
    Ok(value)
}
