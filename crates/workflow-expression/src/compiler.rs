use crate::error::admission;
use crate::{Category, ExpressionError, Limits, Position, Profile};
use cel::Program;
use cel::common::ast::{EntryExpr, Expr, IdedExpr};
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, rc::Rc};

pub(crate) struct OwnedProgram(Program);
impl OwnedProgram {
    fn new(program: Program) -> Self {
        crate::worker::witness("create");
        Self(program)
    }
}
impl std::ops::Deref for OwnedProgram {
    type Target = Program;
    fn deref(&self) -> &Program {
        &self.0
    }
}
impl Drop for OwnedProgram {
    fn drop(&mut self) {
        crate::worker::witness("drop");
    }
}
const FUNCTIONS: &[&str] = &[
    "_?_:_",
    "_&&_",
    "_||_",
    "!_",
    "@not_strictly_false",
    "_+_",
    "_-_",
    "-_",
    "_*_",
    "_/_",
    "_%_",
    "_==_",
    "_!=_",
    "_>_",
    "_>=_",
    "_<_",
    "_<=_",
    "_[_]",
    "@in",
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
];
/// Program internals are intentionally private and excluded from diagnostics.
pub struct Compiled {
    pub(crate) program: Rc<OwnedProgram>,
    pub(crate) position: Position,
    pub(crate) limits: Limits,
}
/// Compile on the declared compilation thread, including rejection and final program cleanup.
/// See the crate's compilation-environment contract; AST checks run after CEL parsing.
/// Uses the same compilation-thread requirement as [`compile`], even with small limits.
/// Only the source-byte bound precedes parsing. AST limits are post-parse validation.
pub fn compile_with_limits(
    source: &str,
    position: Position,
    limits: Limits,
) -> Result<Compiled, ExpressionError> {
    if source.len() > limits.source_bytes {
        return Err(admission(Category::Limit));
    }
    let program =
        OwnedProgram::new(Program::compile(source).map_err(|_| admission(Category::Invalid))?);
    audit(program.expression(), position, limits)?;
    Ok(Compiled {
        program: Rc::new(program),
        position,
        limits,
    })
}
pub(crate) fn audit(
    root: &IdedExpr,
    position: Position,
    limits: Limits,
) -> Result<usize, ExpressionError> {
    // Expanded comprehension locals have lexical scope, not a global exclusion list.
    let mut todo = vec![(root, 1, 0, Vec::<String>::new())];
    let mut nodes = 0;
    while let Some((expr, depth, nesting, locals)) = todo.pop() {
        nodes += 1;
        if nodes > limits.ast_nodes || depth > limits.ast_depth {
            return Err(admission(Category::Limit));
        }
        let mut children = Vec::new();
        match &expr.expr {
            Expr::Ident(name) if !position.allows(name) && !locals.contains(name) => {
                return Err(admission(Category::Invalid));
            }
            Expr::Call(call) => {
                if ["map", "filter", "all", "exists", "exists_one", "has"]
                    .contains(&call.func_name.as_str())
                {
                    return Err(admission(Category::Invalid));
                }
                if !FUNCTIONS.contains(&call.func_name.as_str()) {
                    return Err(admission(Category::Unsupported));
                }
                if let Some(target) = &call.target {
                    children.push(target.as_ref());
                }
                children.extend(call.args.iter());
            }
            Expr::Select(select) => children.push(&select.operand),
            Expr::List(list) => {
                if !list.optional_indices.is_empty() {
                    return Err(admission(Category::Unsupported));
                }
                children.extend(list.elements.iter());
            }
            Expr::Map(map) => {
                for entry in &map.entries {
                    let EntryExpr::MapEntry(entry) = &entry.expr else {
                        return Err(admission(Category::Unsupported));
                    };
                    if entry.optional {
                        return Err(admission(Category::Unsupported));
                    }
                    children.extend([&entry.key, &entry.value]);
                }
            }
            Expr::Comprehension(c) => {
                if nesting >= limits.comprehension_depth {
                    return Err(admission(Category::Limit));
                }
                // iter_range and accu_init evaluate in the parent scope; result sees only accumulator.
                let mut result_scope = locals.clone();
                result_scope.push(c.accu_var.clone());
                let mut loop_scope = result_scope.clone();
                loop_scope.push(c.iter_var.clone());
                if let Some(second) = &c.iter_var2 {
                    loop_scope.push(second.clone());
                }
                todo.push((&c.result, depth + 1, nesting + 1, result_scope));
                todo.push((&c.loop_step, depth + 1, nesting + 1, loop_scope.clone()));
                todo.push((&c.loop_cond, depth + 1, nesting + 1, loop_scope));
                todo.push((&c.accu_init, depth + 1, nesting + 1, locals.clone()));
                todo.push((&c.iter_range, depth + 1, nesting + 1, locals));
                continue;
            }
            Expr::Struct(_) | Expr::Unspecified => return Err(admission(Category::Unsupported)),
            _ => (),
        }
        todo.extend(
            children
                .into_iter()
                .rev()
                .map(|e| (e, depth + 1, nesting, locals.clone())),
        );
    }
    Ok(nodes)
}
struct Entry {
    profile: Profile,
    hash: [u8; 32],
    program: Rc<OwnedProgram>,
    bytes: usize,
}
/// FIFO bounded program-only cache. Validation always runs, including hits.
/// Retention charge conservatively bounds each AST node and every possible source-derived string.
pub struct CompileCache {
    entries: VecDeque<Entry>,
    retired: Vec<Entry>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}
impl Default for CompileCache {
    fn default() -> Self {
        Self::new()
    }
}
impl CompileCache {
    pub const MAX_ENTRIES: usize = 128;
    pub const MAX_BYTES: usize = 16 * 1024 * 1024;
    pub fn new() -> Self {
        Self::with_budget(Self::MAX_ENTRIES, Self::MAX_BYTES)
    }
    /// Configure a smaller cache budget, e.g. to test eviction using ordinary small expressions.
    /// Larger values are clamped to the production caps; a zero budget disables retention.
    /// These are cache-retention budgets, not expression source/AST limits.
    pub fn with_budget(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            retired: Vec::new(),
            bytes: 0,
            max_entries: max_entries.min(Self::MAX_ENTRIES),
            max_bytes: max_bytes.min(Self::MAX_BYTES),
        }
    }
    pub(crate) fn collect(&mut self) {
        let mut i = 0;
        while i < self.retired.len() {
            if Rc::strong_count(&self.retired[i].program) == 1 {
                let entry = self.retired.swap_remove(i);
                let bytes = entry.bytes;
                drop(entry); // final disposal precedes accounting release, on this worker
                self.bytes -= bytes;
            } else {
                i += 1;
            }
        }
    }
    pub fn len(&self) -> usize {
        self.entries.len() + self.retired.len()
    }
    pub fn retained_bytes(&self) -> usize {
        self.bytes
    }
    pub fn compile_with_limits(
        &mut self,
        profile: Profile,
        source: &str,
        position: Position,
        limits: Limits,
    ) -> Result<Compiled, ExpressionError> {
        self.collect();
        if profile != Profile::CelWorkflowV2 {
            return Err(admission(Category::ProfileUnsupported));
        }
        if source.len() > limits.source_bytes {
            return Err(admission(Category::Limit));
        }
        let hash: [u8; 32] = Sha256::digest(source.as_bytes()).into();
        if let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.profile == profile && entry.hash == hash)
        {
            audit(entry.program.expression(), position, limits)?;
            return Ok(Compiled {
                program: entry.program.clone(),
                position,
                limits,
            });
        }
        let compiled = compile_with_limits(source, position, limits)?;
        let nodes = audit(compiled.program.expression(), position, limits)?;
        // CEL AST vectors grow geometrically; 512 bytes/node covers enum, capacity, boxes and fixed macro names.
        // Every node can own source-sized strings (macro expansion duplicates names); charge 4 copies/node.
        let bytes = nodes
            .saturating_mul(512usize.saturating_add(source.len().saturating_mul(4)))
            .saturating_add(256);
        if self.max_entries > 0 && bytes <= self.max_bytes {
            while self.len() >= self.max_entries || self.bytes + bytes > self.max_bytes {
                let Some(entry) = self.entries.pop_front() else {
                    break;
                };
                self.retired.push(entry);
                self.collect();
            }
            if self.len() >= self.max_entries || self.bytes + bytes > self.max_bytes {
                return Ok(compiled);
            }
            self.bytes += bytes;
            self.entries.push_back(Entry {
                profile,
                hash,
                program: compiled.program.clone(),
                bytes,
            });
        }
        Ok(compiled)
    }
}
