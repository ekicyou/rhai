//! Main module defining the script evaluation `Engine`.

use crate::any::{Any, AnyExt, Dynamic, Variant};
use crate::parser::{Expr, FnDef, Position, ReturnType, Stmt, INT};
use crate::result::EvalAltResult;
use crate::scope::{EntryRef as ScopeSource, EntryType as ScopeEntryType, Scope};

#[cfg(not(feature = "no_optimize"))]
use crate::optimize::OptimizationLevel;

use crate::stdlib::{
    any::{type_name, TypeId},
    borrow::Cow,
    boxed::Box,
    cmp::Ordering,
    collections::HashMap,
    format,
    iter::once,
    string::{String, ToString},
    sync::Arc,
    vec,
    vec::Vec,
};

/// An dynamic array of `Dynamic` values.
///
/// Not available under the `no_index` feature.
#[cfg(not(feature = "no_index"))]
pub type Array = Vec<Dynamic>;

/// An dynamic hash map of `Dynamic` values with `String` keys.
///
/// Not available under the `no_object` feature.
#[cfg(not(feature = "no_object"))]
pub type Map = HashMap<String, Dynamic>;

pub type FnCallArgs<'a> = [&'a mut Variant];

#[cfg(feature = "sync")]
pub type FnAny = dyn Fn(&mut FnCallArgs, Position) -> Result<Dynamic, EvalAltResult> + Send + Sync;
#[cfg(not(feature = "sync"))]
pub type FnAny = dyn Fn(&mut FnCallArgs, Position) -> Result<Dynamic, EvalAltResult>;

#[cfg(feature = "sync")]
type IteratorFn = dyn Fn(&Dynamic) -> Box<dyn Iterator<Item = Dynamic>> + Send + Sync;
#[cfg(not(feature = "sync"))]
type IteratorFn = dyn Fn(&Dynamic) -> Box<dyn Iterator<Item = Dynamic>>;

pub const MAX_CALL_STACK_DEPTH: usize = 64;
pub const KEYWORD_PRINT: &str = "print";
pub const KEYWORD_DEBUG: &str = "debug";
pub const KEYWORD_DUMP_AST: &str = "dump_ast";
pub const KEYWORD_TYPE_OF: &str = "type_of";
pub const KEYWORD_EVAL: &str = "eval";
pub const FUNC_TO_STRING: &str = "to_string";
pub const FUNC_GETTER: &str = "get$";
pub const FUNC_SETTER: &str = "set$";

#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
#[cfg(not(feature = "no_index"))]
enum IndexSourceType {
    Expression,
    String,
    Array,
    #[cfg(not(feature = "no_object"))]
    Map,
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
enum IndexValue {
    Num(usize),
    Str(String),
}

impl IndexValue {
    fn from_num(idx: INT) -> Self {
        Self::Num(idx as usize)
    }
    fn from_str(name: String) -> Self {
        Self::Str(name)
    }
    fn as_num(self) -> usize {
        match self {
            Self::Num(n) => n,
            _ => panic!("index value is numeric"),
        }
    }
    fn as_str(self) -> String {
        match self {
            Self::Str(s) => s,
            _ => panic!("index value is string"),
        }
    }
}

#[derive(Debug)]
enum Target<'a> {
    Scope(ScopeSource<'a>),
    Value(&'a mut Variant),
}

impl<'a> Target<'a> {
    fn from(value: &'a mut Variant) -> Self {
        Self::Value(value)
    }
    fn from_src(src: ScopeSource<'a>) -> Self {
        Self::Scope(src)
    }
    fn get_mut(self, scope: &'a mut Scope) -> &'a mut Variant {
        match self {
            Self::Value(t) => t,
            Self::Scope(src) => scope.get_mut(src).as_mut(),
        }
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub struct FnSpec<'a> {
    pub name: Cow<'a, str>,
    pub args: Vec<TypeId>,
}

/// A type that holds a library of script-defined functions.
///
/// Since script-defined functions have `Dynamic` parameters, functions with the same name
/// and number of parameters are considered equivalent.
///
/// Since the key is a combination of the function name (a String) plus the number of parameters,
/// we cannot use a `HashMap` because we don't want to clone the function name string just
/// to search for it.
///
/// So instead this is implemented as a sorted list and binary searched.
#[derive(Debug)]
pub struct FunctionsLib(Vec<Arc<FnDef>>);

impl FnDef {
    /// Function to order two FnDef records, for binary search.
    pub fn compare(&self, name: &str, params_len: usize) -> Ordering {
        // First order by name
        match self.name.as_str().cmp(name) {
            // Then by number of parameters
            Ordering::Equal => self.params.len().cmp(&params_len),
            order => order,
        }
    }
}

impl FunctionsLib {
    /// Create a new `FunctionsLib`.
    pub fn new() -> Self {
        FunctionsLib(Vec::new())
    }
    /// Does a certain function exist in the `FunctionsLib`?
    pub fn has_function(&self, name: &str, params: usize) -> bool {
        self.0.binary_search_by(|f| f.compare(name, params)).is_ok()
    }
    /// Add a function (or replace an existing one) in the `FunctionsLib`.
    pub fn add_or_replace_function(&mut self, fn_def: Arc<FnDef>) {
        match self
            .0
            .binary_search_by(|f| f.compare(&fn_def.name, fn_def.params.len()))
        {
            Ok(n) => self.0[n] = fn_def,
            Err(n) => self.0.insert(n, fn_def),
        }
    }
    /// Get a function definition from the `FunctionsLib`.
    pub fn get_function(&self, name: &str, params: usize) -> Option<Arc<FnDef>> {
        if let Ok(n) = self.0.binary_search_by(|f| f.compare(name, params)) {
            Some(self.0[n].clone())
        } else {
            None
        }
    }
}

/// Rhai main scripting engine.
///
/// ```
/// # fn main() -> Result<(), rhai::EvalAltResult> {
/// use rhai::Engine;
///
/// let mut engine = Engine::new();
///
/// let result = engine.eval::<i64>("40 + 2")?;
///
/// println!("Answer: {}", result);  // prints 42
/// # Ok(())
/// # }
/// ```
///
/// Currently, `Engine` is neither `Send` nor `Sync`. Turn on the `sync` feature to make it `Send + Sync`.
pub struct Engine<'e> {
    /// A hashmap containing all compiled functions known to the engine.
    pub(crate) functions: Option<HashMap<FnSpec<'e>, Box<FnAny>>>,
    /// A hashmap containing all script-defined functions.
    pub(crate) fn_lib: Option<FunctionsLib>,
    /// A hashmap containing all iterators known to the engine.
    pub(crate) type_iterators: Option<HashMap<TypeId, Box<IteratorFn>>>,
    /// A hashmap mapping type names to pretty-print names.
    pub(crate) type_names: Option<HashMap<String, String>>,

    /// Closure for implementing the `print` command.
    #[cfg(feature = "sync")]
    pub(crate) on_print: Option<Box<dyn FnMut(&str) + Send + Sync + 'e>>,
    #[cfg(not(feature = "sync"))]
    pub(crate) on_print: Option<Box<dyn FnMut(&str) + 'e>>,

    /// Closure for implementing the `debug` command.
    #[cfg(feature = "sync")]
    pub(crate) on_debug: Option<Box<dyn FnMut(&str) + Send + Sync + 'e>>,
    #[cfg(not(feature = "sync"))]
    pub(crate) on_debug: Option<Box<dyn FnMut(&str) + 'e>>,

    /// Optimize the AST after compilation.
    #[cfg(not(feature = "no_optimize"))]
    pub(crate) optimization_level: OptimizationLevel,

    /// Maximum levels of call-stack to prevent infinite recursion.
    pub(crate) max_call_stack_depth: usize,
}

impl Default for Engine<'_> {
    fn default() -> Self {
        // User-friendly names for built-in types
        let type_names = [
            #[cfg(not(feature = "no_index"))]
            (type_name::<Array>(), "array"),
            #[cfg(not(feature = "no_object"))]
            (type_name::<Map>(), "map"),
            (type_name::<String>(), "string"),
            (type_name::<Dynamic>(), "dynamic"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        // Create the new scripting Engine
        let mut engine = Engine {
            functions: None,
            fn_lib: None,
            type_iterators: None,
            type_names: Some(type_names),
            on_print: Some(Box::new(default_print)), // default print/debug implementations
            on_debug: Some(Box::new(default_print)),

            #[cfg(not(feature = "no_optimize"))]
            #[cfg(not(feature = "optimize_full"))]
            optimization_level: OptimizationLevel::Simple,

            #[cfg(not(feature = "no_optimize"))]
            #[cfg(feature = "optimize_full")]
            optimization_level: OptimizationLevel::Full,

            max_call_stack_depth: MAX_CALL_STACK_DEPTH,
        };

        engine.register_core_lib();

        #[cfg(not(feature = "no_stdlib"))]
        engine.register_stdlib(); // Register the standard library when no_stdlib is not set

        engine
    }
}

/// Make getter function
pub fn make_getter(id: &str) -> String {
    format!("{}{}", FUNC_GETTER, id)
}

/// Extract the property name from a getter function name.
fn extract_prop_from_getter(fn_name: &str) -> Option<&str> {
    if fn_name.starts_with(FUNC_GETTER) {
        Some(&fn_name[FUNC_GETTER.len()..])
    } else {
        None
    }
}

/// Make setter function
pub fn make_setter(id: &str) -> String {
    format!("{}{}", FUNC_SETTER, id)
}

/// Extract the property name from a setter function name.
fn extract_prop_from_setter(fn_name: &str) -> Option<&str> {
    if fn_name.starts_with(FUNC_SETTER) {
        Some(&fn_name[FUNC_SETTER.len()..])
    } else {
        None
    }
}

impl Engine<'_> {
    /// Create a new `Engine`
    pub fn new() -> Self {
        // fn abc<F: Fn() + Send + Sync>(f: F) {
        //     f();
        // }
        // abc(|| ());

        Default::default()
    }

    /// Create a new `Engine` with minimal configurations - i.e. without pretty-print type names etc.
    pub fn new_raw() -> Self {
        let mut engine = Engine {
            functions: None,
            fn_lib: None,
            type_iterators: None,
            type_names: None,
            on_print: None,
            on_debug: None,

            #[cfg(not(feature = "no_optimize"))]
            #[cfg(not(feature = "optimize_full"))]
            optimization_level: OptimizationLevel::Simple,

            #[cfg(not(feature = "no_optimize"))]
            #[cfg(feature = "optimize_full")]
            optimization_level: OptimizationLevel::Full,

            max_call_stack_depth: MAX_CALL_STACK_DEPTH,
        };

        engine.register_core_lib();

        #[cfg(not(feature = "no_stdlib"))]
        engine.register_stdlib(); // Register the standard library when no_stdlib is not set

        engine
    }

    /// Control whether and how the `Engine` will optimize an AST after compilation
    ///
    /// Not available under the `no_optimize` feature.
    #[cfg(not(feature = "no_optimize"))]
    pub fn set_optimization_level(&mut self, optimization_level: OptimizationLevel) {
        self.optimization_level = optimization_level
    }

    /// Set the maximum levels of function calls allowed for a script in order to avoid
    /// infinite recursion and stack overflows.
    pub fn set_max_call_levels(&mut self, levels: usize) {
        self.max_call_stack_depth = levels
    }

    /// Call a registered function
    #[cfg(not(feature = "no_optimize"))]
    pub(crate) fn call_ext_fn_raw(
        &self,
        fn_name: &str,
        args: &mut FnCallArgs,
        pos: Position,
    ) -> Result<Option<Dynamic>, EvalAltResult> {
        let spec = FnSpec {
            name: fn_name.into(),
            args: args.iter().map(|a| Any::type_id(&**a)).collect(),
        };

        // Search built-in's and external functions
        if let Some(ref functions) = self.functions {
            if let Some(func) = functions.get(&spec) {
                // Run external function
                Ok(Some(func(args, pos)?))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    /// Universal method for calling functions either registered with the `Engine` or written in Rhai
    pub(crate) fn call_fn_raw(
        &mut self,
        fn_name: &str,
        args: &mut FnCallArgs,
        def_val: Option<&Dynamic>,
        pos: Position,
        level: usize,
    ) -> Result<Dynamic, EvalAltResult> {
        // First search in script-defined functions (can override built-in)
        if let Some(ref fn_lib) = self.fn_lib {
            if let Some(fn_def) = fn_lib.get_function(fn_name, args.len()) {
                let mut scope = Scope::new();

                scope.extend(
                    // Put arguments into scope as variables
                    fn_def
                        .params
                        .iter()
                        .zip(args.iter().map(|x| (*x).into_dynamic()))
                        .map(|(name, value)| (name, ScopeEntryType::Normal, value)),
                );

                // Evaluate the function at one higher level of call depth
                return self.eval_stmt(&mut scope, &fn_def.body, level + 1).or_else(
                    |err| match err {
                        // Convert return statement to return value
                        EvalAltResult::Return(x, _) => Ok(x),
                        err => Err(err.set_position(pos)),
                    },
                );
            }
        }

        let spec = FnSpec {
            name: fn_name.into(),
            args: args.iter().map(|a| Any::type_id(&**a)).collect(),
        };

        // Argument must be a string
        fn cast_to_string(r: &Variant, pos: Position) -> Result<&str, EvalAltResult> {
            r.downcast_ref::<String>()
                .map(String::as_str)
                .ok_or_else(|| EvalAltResult::ErrorMismatchOutputType(r.type_name().into(), pos))
        }

        // Search built-in's and external functions
        if let Some(ref functions) = self.functions {
            if let Some(func) = functions.get(&spec) {
                // Run external function
                let result = func(args, pos)?;

                // See if the function match print/debug (which requires special processing)
                return Ok(match fn_name {
                    KEYWORD_PRINT if self.on_print.is_some() => {
                        self.on_print.as_deref_mut().unwrap()(cast_to_string(result.as_ref(), pos)?)
                            .into_dynamic()
                    }
                    KEYWORD_DEBUG if self.on_debug.is_some() => {
                        self.on_debug.as_deref_mut().unwrap()(cast_to_string(result.as_ref(), pos)?)
                            .into_dynamic()
                    }
                    KEYWORD_PRINT | KEYWORD_DEBUG => ().into_dynamic(),
                    _ => result,
                });
            }
        }

        if let Some(prop) = extract_prop_from_getter(fn_name) {
            #[cfg(not(feature = "no_object"))]
            {
                // Map property access
                if let Some(map) = args[0].downcast_ref::<Map>() {
                    return Ok(map.get(prop).cloned().unwrap_or_else(|| ().into_dynamic()));
                }
            }

            // Getter function not found
            return Err(EvalAltResult::ErrorDotExpr(
                format!("- property '{}' unknown or write-only", prop),
                pos,
            ));
        }

        if let Some(prop) = extract_prop_from_setter(fn_name) {
            #[cfg(not(feature = "no_object"))]
            {
                let value = args[1].into_dynamic();

                // Map property update
                if let Some(map) = args[0].downcast_mut::<Map>() {
                    map.insert(prop.to_string(), value);
                    return Ok(().into_dynamic());
                }
            }

            // Setter function not found
            return Err(EvalAltResult::ErrorDotExpr(
                format!("- property '{}' unknown or read-only", prop),
                pos,
            ));
        }

        if let Some(val) = def_val {
            // Return default value
            return Ok(val.clone());
        }

        // Raise error
        let types_list: Vec<_> = args
            .iter()
            .map(|x| (*x).type_name())
            .map(|name| self.map_type_name(name))
            .collect();

        Err(EvalAltResult::ErrorFunctionNotFound(
            format!("{} ({})", fn_name, types_list.join(", ")),
            pos,
        ))
    }

    /// Chain-evaluate a dot setter.
    #[cfg(not(feature = "no_object"))]
    fn get_dot_val_helper(
        &mut self,
        scope: &mut Scope,
        target: Target,
        dot_rhs: &Expr,
        level: usize,
    ) -> Result<Dynamic, EvalAltResult> {
        match dot_rhs {
            // xxx.fn_name(args)
            Expr::FunctionCall(fn_name, arg_expr_list, def_val, pos) => {
                let mut values = arg_expr_list
                    .iter()
                    .map(|arg_expr| self.eval_expr(scope, arg_expr, level))
                    .collect::<Result<Vec<_>, _>>()?;

                let this_ptr = target.get_mut(scope);

                let mut arg_values: Vec<_> = once(this_ptr)
                    .chain(values.iter_mut().map(Dynamic::as_mut))
                    .collect();

                self.call_fn_raw(fn_name, &mut arg_values, def_val.as_ref(), *pos, 0)
            }

            // xxx.id
            Expr::Property(id, pos) => {
                let this_ptr = target.get_mut(scope);
                self.call_fn_raw(&make_getter(id), &mut [this_ptr], None, *pos, 0)
            }

            // xxx.idx_lhs[idx_expr]
            #[cfg(not(feature = "no_index"))]
            Expr::Index(idx_lhs, idx_expr, op_pos) => {
                let value = match idx_lhs.as_ref() {
                    // xxx.id[idx_expr]
                    Expr::Property(id, pos) => {
                        let this_ptr = target.get_mut(scope);
                        self.call_fn_raw(&make_getter(id), &mut [this_ptr], None, *pos, 0)?
                    }
                    // xxx.???[???][idx_expr]
                    Expr::Index(_, _, _) => {
                        self.get_dot_val_helper(scope, target, idx_lhs, level)?
                    }
                    // Syntax error
                    _ => {
                        return Err(EvalAltResult::ErrorDotExpr(
                            "".to_string(),
                            dot_rhs.position(),
                        ))
                    }
                };

                self.get_indexed_value(scope, &value, idx_expr, *op_pos, level)
                    .map(|(val, _, _)| val)
            }

            // xxx.dot_lhs.rhs
            Expr::Dot(dot_lhs, rhs, _) => match dot_lhs.as_ref() {
                // xxx.id.rhs
                Expr::Property(id, pos) => {
                    let this_ptr = target.get_mut(scope);
                    self.call_fn_raw(&make_getter(id), &mut [this_ptr], None, *pos, 0)
                        .and_then(|mut val| {
                            self.get_dot_val_helper(scope, Target::from(val.as_mut()), rhs, level)
                        })
                }
                // xxx.idx_lhs[idx_expr].rhs
                #[cfg(not(feature = "no_index"))]
                Expr::Index(idx_lhs, idx_expr, op_pos) => {
                    let val = match idx_lhs.as_ref() {
                        // xxx.id[idx_expr].rhs
                        Expr::Property(id, pos) => {
                            let this_ptr = target.get_mut(scope);
                            self.call_fn_raw(&make_getter(id), &mut [this_ptr], None, *pos, 0)?
                        }
                        // xxx.???[???][idx_expr].rhs
                        Expr::Index(_, _, _) => {
                            self.get_dot_val_helper(scope, target, idx_lhs, level)?
                        }
                        // Syntax error
                        _ => {
                            return Err(EvalAltResult::ErrorDotExpr(
                                "".to_string(),
                                dot_rhs.position(),
                            ))
                        }
                    };

                    self.get_indexed_value(scope, &val, idx_expr, *op_pos, level)
                        .and_then(|(mut val, _, _)| {
                            self.get_dot_val_helper(scope, Target::from(val.as_mut()), rhs, level)
                        })
                }
                // Syntax error
                _ => Err(EvalAltResult::ErrorDotExpr(
                    "".to_string(),
                    dot_lhs.position(),
                )),
            },

            // Syntax error
            _ => Err(EvalAltResult::ErrorDotExpr(
                "".to_string(),
                dot_rhs.position(),
            )),
        }
    }

    /// Evaluate a dot chain getter
    #[cfg(not(feature = "no_object"))]
    fn get_dot_val(
        &mut self,
        scope: &mut Scope,
        dot_lhs: &Expr,
        dot_rhs: &Expr,
        level: usize,
    ) -> Result<Dynamic, EvalAltResult> {
        match dot_lhs {
            // id.???
            Expr::Variable(id, pos) => {
                let (entry, _) = Self::search_scope(scope, id, *pos)?;

                // Avoid referencing scope which is used below as mut
                let entry = ScopeSource { name: id, ..entry };

                // This is a variable property access (potential function call).
                // Use a direct index into `scope` to directly mutate the variable value.
                self.get_dot_val_helper(scope, Target::from_src(entry), dot_rhs, level)
            }

            // idx_lhs[idx_expr].???
            #[cfg(not(feature = "no_index"))]
            Expr::Index(idx_lhs, idx_expr, op_pos) => {
                let (idx_src_type, src, idx, mut val) =
                    self.eval_index_expr(scope, idx_lhs, idx_expr, *op_pos, level)?;
                let value =
                    self.get_dot_val_helper(scope, Target::from(val.as_mut()), dot_rhs, level);

                // In case the expression mutated `target`, we need to update it back into the scope because it is cloned.
                if let Some(src) = src {
                    match src.typ {
                        ScopeEntryType::Constant => {
                            return Err(EvalAltResult::ErrorAssignmentToConstant(
                                src.name.to_string(),
                                idx_lhs.position(),
                            ));
                        }
                        ScopeEntryType::Normal => {
                            Self::update_indexed_var_in_scope(
                                idx_src_type,
                                scope,
                                src,
                                idx,
                                (val, dot_rhs.position()),
                            )?;
                        }
                    }
                }

                value
            }

            // {expr}.???
            expr => {
                let mut val = self.eval_expr(scope, expr, level)?;
                self.get_dot_val_helper(scope, Target::from(val.as_mut()), dot_rhs, level)
            }
        }
    }

    /// Search for a variable within the scope, returning its value and index inside the Scope
    fn search_scope<'a>(
        scope: &'a Scope,
        id: &str,
        begin: Position,
    ) -> Result<(ScopeSource<'a>, Dynamic), EvalAltResult> {
        scope
            .get(id)
            .ok_or_else(|| EvalAltResult::ErrorVariableNotFound(id.into(), begin))
    }

    /// Get the value at the indexed position of a base type
    #[cfg(not(feature = "no_index"))]
    fn get_indexed_value(
        &mut self,
        scope: &mut Scope,
        val: &Dynamic,
        idx_expr: &Expr,
        op_pos: Position,
        level: usize,
    ) -> Result<(Dynamic, IndexSourceType, IndexValue), EvalAltResult> {
        let idx_pos = idx_expr.position();

        // val_array[idx]
        if let Some(arr) = val.downcast_ref::<Array>() {
            let idx = self
                .eval_expr(scope, idx_expr, level)?
                .try_cast::<INT>()
                .map_err(|_| EvalAltResult::ErrorNumericIndexExpr(idx_expr.position()))?;

            return if idx >= 0 {
                arr.get(idx as usize)
                    .cloned()
                    .map(|v| (v, IndexSourceType::Array, IndexValue::from_num(idx)))
                    .ok_or_else(|| EvalAltResult::ErrorArrayBounds(arr.len(), idx, idx_pos))
            } else {
                Err(EvalAltResult::ErrorArrayBounds(arr.len(), idx, idx_pos))
            };
        }

        #[cfg(not(feature = "no_object"))]
        {
            // val_map[idx]
            if let Some(map) = val.downcast_ref::<Map>() {
                let idx = self
                    .eval_expr(scope, idx_expr, level)?
                    .try_cast::<String>()
                    .map_err(|_| EvalAltResult::ErrorStringIndexExpr(idx_expr.position()))?;

                return Ok((
                    map.get(&idx).cloned().unwrap_or_else(|| ().into_dynamic()),
                    IndexSourceType::Map,
                    IndexValue::from_str(idx),
                ));
            }
        }

        // val_string[idx]
        if let Some(s) = val.downcast_ref::<String>() {
            let idx = self
                .eval_expr(scope, idx_expr, level)?
                .try_cast::<INT>()
                .map_err(|_| EvalAltResult::ErrorNumericIndexExpr(idx_expr.position()))?;

            return if idx >= 0 {
                s.chars()
                    .nth(idx as usize)
                    .map(|ch| {
                        (
                            ch.into_dynamic(),
                            IndexSourceType::String,
                            IndexValue::from_num(idx),
                        )
                    })
                    .ok_or_else(|| {
                        EvalAltResult::ErrorStringBounds(s.chars().count(), idx, idx_pos)
                    })
            } else {
                Err(EvalAltResult::ErrorStringBounds(
                    s.chars().count(),
                    idx,
                    idx_pos,
                ))
            };
        }

        // Error - cannot be indexed
        Err(EvalAltResult::ErrorIndexingType(
            self.map_type_name(val.type_name()).to_string(),
            op_pos,
        ))
    }

    /// Evaluate an index expression
    #[cfg(not(feature = "no_index"))]
    fn eval_index_expr<'a>(
        &mut self,
        scope: &mut Scope,
        lhs: &'a Expr,
        idx_expr: &Expr,
        op_pos: Position,
        level: usize,
    ) -> Result<
        (
            IndexSourceType,
            Option<ScopeSource<'a>>,
            IndexValue,
            Dynamic,
        ),
        EvalAltResult,
    > {
        match lhs {
            // id[idx_expr]
            Expr::Variable(id, _) => {
                let (
                    ScopeSource {
                        typ: src_type,
                        index: src_idx,
                        ..
                    },
                    val,
                ) = Self::search_scope(scope, &id, lhs.position())?;

                let (val, idx_src_type, idx) =
                    self.get_indexed_value(scope, &val, idx_expr, op_pos, level)?;

                Ok((
                    idx_src_type,
                    Some(ScopeSource {
                        name: &id,
                        typ: src_type,
                        index: src_idx,
                    }),
                    idx,
                    val,
                ))
            }

            // (expr)[idx_expr]
            expr => {
                let val = self.eval_expr(scope, expr, level)?;

                self.get_indexed_value(scope, &val, idx_expr, op_pos, level)
                    .map(|(val, _, idx)| (IndexSourceType::Expression, None, idx, val))
            }
        }
    }

    /// Replace a character at an index position in a mutable string
    #[cfg(not(feature = "no_index"))]
    fn str_replace_char(s: &mut String, idx: usize, new_ch: char) {
        let mut chars: Vec<char> = s.chars().collect();
        let ch = *chars.get(idx).expect("string index out of bounds");

        // See if changed - if so, update the String
        if ch != new_ch {
            chars[idx] = new_ch;
            s.clear();
            chars.iter().for_each(|&ch| s.push(ch));
        }
    }

    /// Update the value at an index position in a variable inside the scope
    #[cfg(not(feature = "no_index"))]
    fn update_indexed_var_in_scope(
        idx_src_type: IndexSourceType,
        scope: &mut Scope,
        src: ScopeSource,
        idx: IndexValue,
        new_val: (Dynamic, Position),
    ) -> Result<Dynamic, EvalAltResult> {
        match idx_src_type {
            // array_id[idx] = val
            IndexSourceType::Array => {
                let arr = scope.get_mut_by_type::<Array>(src);
                arr[idx.as_num()] = new_val.0;
                Ok(().into_dynamic())
            }

            // map_id[idx] = val
            #[cfg(not(feature = "no_object"))]
            IndexSourceType::Map => {
                let arr = scope.get_mut_by_type::<Map>(src);
                arr.insert(idx.as_str(), new_val.0);
                Ok(().into_dynamic())
            }

            // string_id[idx] = val
            IndexSourceType::String => {
                let s = scope.get_mut_by_type::<String>(src);
                let pos = new_val.1;
                // Value must be a character
                let ch = new_val
                    .0
                    .try_cast::<char>()
                    .map_err(|_| EvalAltResult::ErrorCharMismatch(pos))?;
                Self::str_replace_char(s, idx.as_num(), ch);
                Ok(().into_dynamic())
            }

            IndexSourceType::Expression => panic!("expression cannot be indexed for update"),
        }
    }

    /// Update the value at an index position
    #[cfg(not(feature = "no_index"))]
    fn update_indexed_value(
        mut target: Dynamic,
        idx: IndexValue,
        new_val: Dynamic,
        pos: Position,
    ) -> Result<Dynamic, EvalAltResult> {
        if let Some(arr) = target.downcast_mut::<Array>() {
            arr[idx.as_num()] = new_val;
            return Ok(target);
        }

        #[cfg(not(feature = "no_object"))]
        {
            if let Some(map) = target.downcast_mut::<Map>() {
                map.insert(idx.as_str(), new_val);
                return Ok(target);
            }
        }

        if let Some(s) = target.downcast_mut::<String>() {
            // Value must be a character
            let ch = new_val
                .try_cast::<char>()
                .map_err(|_| EvalAltResult::ErrorCharMismatch(pos))?;
            Self::str_replace_char(s, idx.as_num(), ch);
            return Ok(target);
        }

        // All other variable types should be an error
        panic!("array, map or string source type expected for indexing")
    }

    /// Chain-evaluate a dot setter
    #[cfg(not(feature = "no_object"))]
    fn set_dot_val_helper(
        &mut self,
        scope: &mut Scope,
        this_ptr: &mut Variant,
        dot_rhs: &Expr,
        new_val: (&mut Dynamic, Position),
        level: usize,
    ) -> Result<Dynamic, EvalAltResult> {
        match dot_rhs {
            // xxx.id
            Expr::Property(id, pos) => {
                let mut args = [this_ptr, new_val.0.as_mut()];
                self.call_fn_raw(&make_setter(id), &mut args, None, *pos, 0)
            }

            // xxx.lhs[idx_expr]
            // TODO - Allow chaining of indexing!
            #[cfg(not(feature = "no_index"))]
            Expr::Index(lhs, idx_expr, op_pos) => match lhs.as_ref() {
                // xxx.id[idx_expr]
                Expr::Property(id, pos) => self
                    .call_fn_raw(&make_getter(id), &mut [this_ptr], None, *pos, 0)
                    .and_then(|val| {
                        let (_, _, idx) =
                            self.get_indexed_value(scope, &val, idx_expr, *op_pos, level)?;

                        Self::update_indexed_value(val, idx, new_val.0.clone(), new_val.1)
                    })
                    .and_then(|mut val| {
                        let mut args = [this_ptr, val.as_mut()];
                        self.call_fn_raw(&make_setter(id), &mut args, None, *pos, 0)
                    }),

                // All others - syntax error for setters chain
                _ => Err(EvalAltResult::ErrorDotExpr(
                    "for assignment".to_string(),
                    *op_pos,
                )),
            },

            // xxx.lhs.{...}
            Expr::Dot(lhs, rhs, _) => match lhs.as_ref() {
                // xxx.id.rhs
                Expr::Property(id, pos) => {
                    self.call_fn_raw(&make_getter(id), &mut [this_ptr], None, *pos, 0)
                        .and_then(|mut val| {
                            self.set_dot_val_helper(scope, val.as_mut(), rhs, new_val, level)
                                .map(|_| val) // Discard Ok return value
                        })
                        .and_then(|mut val| {
                            let mut args = [this_ptr, val.as_mut()];
                            self.call_fn_raw(&make_setter(id), &mut args, None, *pos, 0)
                        })
                }

                // xxx.lhs[idx_expr].rhs
                // TODO - Allow chaining of indexing!
                #[cfg(not(feature = "no_index"))]
                Expr::Index(lhs, idx_expr, op_pos) => match lhs.as_ref() {
                    // xxx.id[idx_expr].rhs
                    Expr::Property(id, pos) => {
                        self.call_fn_raw(&make_getter(id), &mut [this_ptr], None, *pos, 0)
                            .and_then(|v| {
                                let (mut value, _, idx) =
                                    self.get_indexed_value(scope, &v, idx_expr, *op_pos, level)?;

                                let val_pos = new_val.1;
                                let this_ptr = value.as_mut();
                                self.set_dot_val_helper(scope, this_ptr, rhs, new_val, level)?;

                                // In case the expression mutated `target`, we need to update it back into the scope because it is cloned.
                                Self::update_indexed_value(v, idx, value, val_pos)
                            })
                            .and_then(|mut v| {
                                self.call_fn_raw(
                                    &make_setter(id),
                                    &mut [this_ptr, v.as_mut()],
                                    None,
                                    *pos,
                                    0,
                                )
                            })
                    }

                    // All others - syntax error for setters chain
                    _ => Err(EvalAltResult::ErrorDotExpr(
                        "for assignment".to_string(),
                        *op_pos,
                    )),
                },

                // All others - syntax error for setters chain
                _ => Err(EvalAltResult::ErrorDotExpr(
                    "for assignment".to_string(),
                    lhs.position(),
                )),
            },

            // Syntax error
            _ => Err(EvalAltResult::ErrorDotExpr(
                "for assignment".to_string(),
                dot_rhs.position(),
            )),
        }
    }

    // Evaluate a dot chain setter
    #[cfg(not(feature = "no_object"))]
    fn set_dot_val(
        &mut self,
        scope: &mut Scope,
        dot_lhs: &Expr,
        dot_rhs: &Expr,
        new_val: (&mut Dynamic, Position),
        op_pos: Position,
        level: usize,
    ) -> Result<Dynamic, EvalAltResult> {
        match dot_lhs {
            // id.???
            Expr::Variable(id, pos) => {
                let (entry, mut target) = Self::search_scope(scope, id, *pos)?;

                match entry.typ {
                    ScopeEntryType::Constant => Err(EvalAltResult::ErrorAssignmentToConstant(
                        id.to_string(),
                        op_pos,
                    )),
                    _ => {
                        // Avoid referencing scope which is used below as mut
                        let entry = ScopeSource { name: id, ..entry };
                        let this_ptr = target.as_mut();
                        let value =
                            self.set_dot_val_helper(scope, this_ptr, dot_rhs, new_val, level);

                        // In case the expression mutated `target`, we need to update it back into the scope because it is cloned.
                        *scope.get_mut(entry) = target;

                        value
                    }
                }
            }

            // lhs[idx_expr].???
            // TODO - Allow chaining of indexing!
            #[cfg(not(feature = "no_index"))]
            Expr::Index(lhs, idx_expr, op_pos) => {
                let (idx_src_type, src, idx, mut target) =
                    self.eval_index_expr(scope, lhs, idx_expr, *op_pos, level)?;
                let val_pos = new_val.1;
                let this_ptr = target.as_mut();
                let value = self.set_dot_val_helper(scope, this_ptr, dot_rhs, new_val, level);

                // In case the expression mutated `target`, we need to update it back into the scope because it is cloned.
                if let Some(src) = src {
                    match src.typ {
                        ScopeEntryType::Constant => {
                            return Err(EvalAltResult::ErrorAssignmentToConstant(
                                src.name.to_string(),
                                lhs.position(),
                            ));
                        }
                        ScopeEntryType::Normal => {
                            Self::update_indexed_var_in_scope(
                                idx_src_type,
                                scope,
                                src,
                                idx,
                                (target, val_pos),
                            )?;
                        }
                    }
                }

                value
            }

            // Syntax error
            _ => Err(EvalAltResult::ErrorDotExpr(
                "for assignment".to_string(),
                dot_lhs.position(),
            )),
        }
    }

    /// Evaluate an expression
    fn eval_expr(
        &mut self,
        scope: &mut Scope,
        expr: &Expr,
        level: usize,
    ) -> Result<Dynamic, EvalAltResult> {
        match expr {
            #[cfg(not(feature = "no_float"))]
            Expr::FloatConstant(f, _) => Ok(f.into_dynamic()),

            Expr::IntegerConstant(i, _) => Ok(i.into_dynamic()),
            Expr::StringConstant(s, _) => Ok(s.into_dynamic()),
            Expr::CharConstant(c, _) => Ok(c.into_dynamic()),
            Expr::Variable(id, pos) => Self::search_scope(scope, id, *pos).map(|(_, val)| val),
            Expr::Property(_, _) => panic!("unexpected property."),

            // lhs[idx_expr]
            #[cfg(not(feature = "no_index"))]
            Expr::Index(lhs, idx_expr, op_pos) => self
                .eval_index_expr(scope, lhs, idx_expr, *op_pos, level)
                .map(|(_, _, _, x)| x),

            // Statement block
            Expr::Stmt(stmt, _) => self.eval_stmt(scope, stmt, level),

            // lhs = rhs
            Expr::Assignment(lhs, rhs, op_pos) => {
                let mut rhs_val = self.eval_expr(scope, rhs, level)?;

                match lhs.as_ref() {
                    // name = rhs
                    Expr::Variable(name, pos) => match scope
                        .get(name)
                        .ok_or_else(|| EvalAltResult::ErrorVariableNotFound(name.clone(), *pos))?
                        .0
                    {
                        entry
                        @
                        ScopeSource {
                            typ: ScopeEntryType::Normal,
                            ..
                        } => {
                            // Avoid referencing scope which is used below as mut
                            let entry = ScopeSource { name, ..entry };

                            *scope.get_mut(entry) = rhs_val.clone();
                            Ok(rhs_val)
                        }
                        ScopeSource {
                            typ: ScopeEntryType::Constant,
                            ..
                        } => Err(EvalAltResult::ErrorAssignmentToConstant(
                            name.to_string(),
                            *op_pos,
                        )),
                    },

                    // idx_lhs[idx_expr] = rhs
                    #[cfg(not(feature = "no_index"))]
                    Expr::Index(idx_lhs, idx_expr, op_pos) => {
                        let (idx_src_type, src, idx, _) =
                            self.eval_index_expr(scope, idx_lhs, idx_expr, *op_pos, level)?;

                        if let Some(src) = src {
                            match src.typ {
                                ScopeEntryType::Constant => {
                                    Err(EvalAltResult::ErrorAssignmentToConstant(
                                        src.name.to_string(),
                                        idx_lhs.position(),
                                    ))
                                }
                                ScopeEntryType::Normal => Ok(Self::update_indexed_var_in_scope(
                                    idx_src_type,
                                    scope,
                                    src,
                                    idx,
                                    (rhs_val, rhs.position()),
                                )?),
                            }
                        } else {
                            Err(EvalAltResult::ErrorAssignmentToUnknownLHS(
                                idx_lhs.position(),
                            ))
                        }
                    }

                    // dot_lhs.dot_rhs = rhs
                    #[cfg(not(feature = "no_object"))]
                    Expr::Dot(dot_lhs, dot_rhs, _) => self.set_dot_val(
                        scope,
                        dot_lhs,
                        dot_rhs,
                        (&mut rhs_val, rhs.position()),
                        *op_pos,
                        level,
                    ),

                    // Error assignment to constant
                    expr if expr.is_constant() => Err(EvalAltResult::ErrorAssignmentToConstant(
                        expr.get_constant_str(),
                        lhs.position(),
                    )),

                    // Syntax error
                    _ => Err(EvalAltResult::ErrorAssignmentToUnknownLHS(lhs.position())),
                }
            }

            #[cfg(not(feature = "no_object"))]
            Expr::Dot(lhs, rhs, _) => self.get_dot_val(scope, lhs, rhs, level),

            #[cfg(not(feature = "no_index"))]
            Expr::Array(contents, _) => {
                let mut arr = Array::new();

                contents.into_iter().try_for_each(|item| {
                    self.eval_expr(scope, item, level).map(|val| arr.push(val))
                })?;

                Ok(Box::new(arr))
            }

            #[cfg(not(feature = "no_object"))]
            Expr::Map(contents, _) => {
                let mut map = Map::new();

                contents.into_iter().try_for_each(|item| {
                    self.eval_expr(scope, &item.1, level).map(|val| {
                        map.insert(item.0.clone(), val);
                    })
                })?;

                Ok(Box::new(map))
            }

            Expr::FunctionCall(fn_name, args_expr_list, def_val, pos) => {
                // Has a system function an override?
                fn has_override(engine: &Engine, name: &str) -> bool {
                    (engine.functions.is_some() && {
                        engine.functions.as_ref().unwrap().contains_key(&FnSpec {
                            name: name.into(),
                            args: vec![TypeId::of::<String>()],
                        })
                    }) || (engine.fn_lib.is_some()
                        && engine.fn_lib.as_ref().unwrap().has_function(name, 1))
                }

                match fn_name.as_str() {
                    // Dump AST
                    KEYWORD_DUMP_AST => {
                        let pos = if args_expr_list.is_empty() {
                            *pos
                        } else {
                            args_expr_list[0].position()
                        };

                        // Change the argument to a debug dump of the expressions
                        let mut result = args_expr_list
                            .iter()
                            .map(|expr| format!("{:#?}", expr))
                            .collect::<Vec<_>>()
                            .join("\n")
                            .into_dynamic();

                        // Redirect call to `print`
                        self.call_fn_raw(KEYWORD_PRINT, &mut [result.as_mut()], None, pos, level)
                    }

                    // type_of
                    KEYWORD_TYPE_OF
                        if args_expr_list.len() == 1 && !has_override(self, KEYWORD_TYPE_OF) =>
                    {
                        let r = self.eval_expr(scope, &args_expr_list[0], level)?;
                        Ok(self
                            .map_type_name((*r).type_name())
                            .to_string()
                            .into_dynamic())
                    }

                    // eval
                    KEYWORD_EVAL
                        if args_expr_list.len() == 1 && !has_override(self, KEYWORD_EVAL) =>
                    {
                        let pos = args_expr_list[0].position();
                        let r = self.eval_expr(scope, &args_expr_list[0], level)?;

                        let script =
                            r.downcast_ref::<String>()
                                .map(String::as_str)
                                .ok_or_else(|| {
                                    EvalAltResult::ErrorMismatchOutputType(
                                        r.type_name().into(),
                                        pos,
                                    )
                                })?;

                        #[cfg(not(feature = "no_optimize"))]
                        let ast = {
                            let orig_optimization_level = self.optimization_level;

                            self.set_optimization_level(OptimizationLevel::None);
                            let ast = self.compile(script);
                            self.set_optimization_level(orig_optimization_level);

                            ast.map_err(EvalAltResult::ErrorParsing)?
                        };

                        #[cfg(feature = "no_optimize")]
                        let ast = self.compile(script).map_err(EvalAltResult::ErrorParsing)?;

                        Ok(self
                            .eval_ast_with_scope_raw(scope, true, &ast)
                            .map_err(|err| err.set_position(pos))?)
                    }

                    // Normal function call
                    _ => {
                        let mut values = args_expr_list
                            .iter()
                            .map(|expr| self.eval_expr(scope, expr, level))
                            .collect::<Result<Vec<_>, _>>()?;

                        let mut arg_values: Vec<_> =
                            values.iter_mut().map(Dynamic::as_mut).collect();

                        self.call_fn_raw(fn_name, &mut arg_values, def_val.as_ref(), *pos, level)
                    }
                }
            }

            Expr::And(lhs, rhs) => Ok(Box::new(
                self
                    .eval_expr(scope, &*lhs, level)?
                    .try_cast::<bool>()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("AND".into(), lhs.position())
                    })?
                    && // Short-circuit using &&
                self
                    .eval_expr(scope, &*rhs, level)?
                    .try_cast::<bool>()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("AND".into(), rhs.position())
                    })?,
            )),

            Expr::Or(lhs, rhs) => Ok(Box::new(
                self
                    .eval_expr(scope, &*lhs, level)?
                    .try_cast::<bool>()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("OR".into(), lhs.position())
                    })?
                    || // Short-circuit using ||
                self
                    .eval_expr(scope, &*rhs, level)?
                    .try_cast::<bool>()
                    .map_err(|_| {
                        EvalAltResult::ErrorBooleanArgMismatch("OR".into(), rhs.position())
                    })?,
            )),

            Expr::True(_) => Ok(true.into_dynamic()),
            Expr::False(_) => Ok(false.into_dynamic()),
            Expr::Unit(_) => Ok(().into_dynamic()),
        }
    }

    /// Evaluate a statement
    pub(crate) fn eval_stmt(
        &mut self,
        scope: &mut Scope,
        stmt: &Stmt,
        level: usize,
    ) -> Result<Dynamic, EvalAltResult> {
        match stmt {
            // No-op
            Stmt::Noop(_) => Ok(().into_dynamic()),

            // Expression as statement
            Stmt::Expr(expr) => {
                let result = self.eval_expr(scope, expr, level)?;

                Ok(if !matches!(expr.as_ref(), Expr::Assignment(_, _, _)) {
                    result
                } else {
                    // If it is an assignment, erase the result at the root
                    ().into_dynamic()
                })
            }

            // Block scope
            Stmt::Block(block, _) => {
                let prev_len = scope.len();

                let result = block.iter().try_fold(().into_dynamic(), |_, stmt| {
                    self.eval_stmt(scope, stmt, level)
                });

                scope.rewind(prev_len);

                result
            }

            // If-else statement
            Stmt::IfThenElse(guard, if_body, else_body) => self
                .eval_expr(scope, guard, level)?
                .try_cast::<bool>()
                .map_err(|_| EvalAltResult::ErrorLogicGuard(guard.position()))
                .and_then(|guard_val| {
                    if guard_val {
                        self.eval_stmt(scope, if_body, level)
                    } else if let Some(stmt) = else_body {
                        self.eval_stmt(scope, stmt.as_ref(), level)
                    } else {
                        Ok(().into_dynamic())
                    }
                }),

            // While loop
            Stmt::While(guard, body) => loop {
                match self.eval_expr(scope, guard, level)?.try_cast::<bool>() {
                    Ok(guard_val) if guard_val => match self.eval_stmt(scope, body, level) {
                        Ok(_) | Err(EvalAltResult::ErrorLoopBreak(false, _)) => (),
                        Err(EvalAltResult::ErrorLoopBreak(true, _)) => return Ok(().into_dynamic()),
                        Err(x) => return Err(x),
                    },
                    Ok(_) => return Ok(().into_dynamic()),
                    Err(_) => return Err(EvalAltResult::ErrorLogicGuard(guard.position())),
                }
            },

            // Loop statement
            Stmt::Loop(body) => loop {
                match self.eval_stmt(scope, body, level) {
                    Ok(_) | Err(EvalAltResult::ErrorLoopBreak(false, _)) => (),
                    Err(EvalAltResult::ErrorLoopBreak(true, _)) => return Ok(().into_dynamic()),
                    Err(x) => return Err(x),
                }
            },

            // For loop
            Stmt::For(name, expr, body) => {
                let arr = self.eval_expr(scope, expr, level)?;
                let tid = Any::type_id(&*arr);

                if let Some(ref type_iterators) = self.type_iterators {
                    if let Some(iter_fn) = type_iterators.get(&tid) {
                        scope.push(name.clone(), ());

                        let entry = ScopeSource {
                            name,
                            index: scope.len() - 1,
                            typ: ScopeEntryType::Normal,
                        };

                        for a in iter_fn(&arr) {
                            *scope.get_mut(entry) = a;

                            match self.eval_stmt(scope, body, level) {
                                Ok(_) | Err(EvalAltResult::ErrorLoopBreak(false, _)) => (),
                                Err(EvalAltResult::ErrorLoopBreak(true, _)) => break,
                                Err(x) => return Err(x),
                            }
                        }

                        scope.rewind(scope.len() - 1);
                        Ok(().into_dynamic())
                    } else {
                        Err(EvalAltResult::ErrorFor(expr.position()))
                    }
                } else {
                    Err(EvalAltResult::ErrorFor(expr.position()))
                }
            }

            // Continue statement
            Stmt::Continue(pos) => Err(EvalAltResult::ErrorLoopBreak(false, *pos)),

            // Break statement
            Stmt::Break(pos) => Err(EvalAltResult::ErrorLoopBreak(true, *pos)),

            // Empty return
            Stmt::ReturnWithVal(None, ReturnType::Return, pos) => {
                Err(EvalAltResult::Return(().into_dynamic(), *pos))
            }

            // Return value
            Stmt::ReturnWithVal(Some(a), ReturnType::Return, pos) => Err(EvalAltResult::Return(
                self.eval_expr(scope, a, level)?,
                *pos,
            )),

            // Empty throw
            Stmt::ReturnWithVal(None, ReturnType::Exception, pos) => {
                Err(EvalAltResult::ErrorRuntime("".into(), *pos))
            }

            // Throw value
            Stmt::ReturnWithVal(Some(a), ReturnType::Exception, pos) => {
                let val = self.eval_expr(scope, a, level)?;
                Err(EvalAltResult::ErrorRuntime(
                    val.try_cast::<String>().unwrap_or_else(|_| "".to_string()),
                    *pos,
                ))
            }

            // Let statement
            Stmt::Let(name, Some(expr), _) => {
                let val = self.eval_expr(scope, expr, level)?;
                scope.push_dynamic_value(name.clone(), ScopeEntryType::Normal, val, false);
                Ok(().into_dynamic())
            }

            Stmt::Let(name, None, _) => {
                scope.push(name.clone(), ());
                Ok(().into_dynamic())
            }

            // Const statement
            Stmt::Const(name, expr, _) if expr.is_constant() => {
                let val = self.eval_expr(scope, expr, level)?;
                scope.push_dynamic_value(name.clone(), ScopeEntryType::Constant, val, true);
                Ok(().into_dynamic())
            }

            Stmt::Const(_, _, _) => panic!("constant expression not constant!"),
        }
    }

    /// Map a type_name into a pretty-print name
    pub(crate) fn map_type_name<'a>(&'a self, name: &'a str) -> &'a str {
        if self.type_names.is_none() {
            name
        } else {
            self.type_names
                .as_ref()
                .unwrap()
                .get(name)
                .map(String::as_str)
                .unwrap_or(name)
        }
    }

    /// Clean up all script-defined functions within the `Engine`.
    pub fn clear_functions(&mut self) {
        self.fn_lib = None;
    }
}

/// Print/debug to stdout
#[cfg(not(feature = "no_std"))]
#[cfg(not(feature = "no_stdlib"))]
fn default_print(s: &str) {
    println!("{}", s);
}

/// No-op
#[cfg(any(feature = "no_std", feature = "no_stdlib"))]
fn default_print(_: &str) {}
