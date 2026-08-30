//! ScalaScript runtime value enum (rust target).
//! Emitted verbatim by RustGen; do not edit by hand.

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
    Tuple(Vec<Value>),
    List(Vec<Value>),
    // A reactive signal: its name (for client-side `data-ssc-*` wiring) + current value.
    Signal(String, Box<Value>),
    // A case-class value living inside an `Any`: constructor name + fields in DECLARATION
    // order. ScalaScript code written against `Any` (std/json-core is the reason this exists:
    // `case class JsonCoreArray(items: List[Any])`) needs somewhere to put a user struct, and
    // this enum is what `Any` maps to. Positional, not named: the walker knows the field
    // order from the `case class` declaration, and carrying names would double the size of
    // every node for information it already has.
    Obj(&'static str, Vec<Value>),
    // A JSON object, and a `Map` generally. Insertion order, not sorted: every other lane
    // prints `Map(a -> 1, b -> 2)` in the order the keys arrived, and a `BTreeMap` here would
    // have quietly reordered a user's data on one lane only.
    Map(Vec<(String, Value)>),
}

// ── Reactive signal store + computed-signal recompute (live server state) ──────
// String-valued (matches the `data-ssc-text` client wiring). Lives here so both
// `Value::signal_value` (read) and the serve runtime (read/write) reach it. Dead
// code for non-serve programs.
#[allow(dead_code)]
static SSC_SIGNALS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, String>>> =
    std::sync::OnceLock::new();
#[allow(dead_code)]
pub fn ssc_signals() -> &'static std::sync::Mutex<std::collections::HashMap<String, String>> {
    SSC_SIGNALS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}
// Computed signals registered for recompute: (generated-name, re-runnable closure).
type SscComputed = Vec<(String, Box<dyn Fn() -> String + Send + Sync>)>;
#[allow(dead_code)]
static SSC_COMPUTED: std::sync::OnceLock<std::sync::Mutex<SscComputed>> = std::sync::OnceLock::new();
#[allow(dead_code)]
fn ssc_computed() -> &'static std::sync::Mutex<SscComputed> {
    SSC_COMPUTED.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}
#[allow(dead_code)]
static SSC_COMPUTED_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
// Register a computed signal: evaluate once for SSR, seed the store, return its name.
#[allow(dead_code)]
pub fn ssc_register_computed(f: Box<dyn Fn() -> String + Send + Sync>) -> String {
    let n = SSC_COMPUTED_N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let name = format!("__c{}", n);
    let v = f();
    ssc_signals().lock().unwrap().insert(name.clone(), v);
    ssc_computed().lock().unwrap().push((name.clone(), f));
    name
}
// Re-run every computed closure (its reads now see the updated store) + write back.
#[allow(dead_code)]
pub fn ssc_recompute_all() {
    let results: Vec<(String, String)> = {
        let reg = ssc_computed().lock().unwrap();
        reg.iter().map(|(n, f)| (n.clone(), f())).collect()
    };
    let mut store = ssc_signals().lock().unwrap();
    for (n, v) in results { store.insert(n, v); }
}

impl Value {
    pub fn show(&self) -> String {
        match self {
            Value::Unit       => "()".to_string(),
            Value::Bool(b)    => b.to_string(),
            Value::Int(n)     => n.to_string(),
            Value::Double(f)  => format_double(*f),
            Value::Str(s)     => s.clone(),
            Value::Tuple(xs)  => render_seq("(", ")", xs),
            Value::List(xs)   => render_seq("List(", ")", xs),
            Value::Signal(_, v) => v.show(),
            // `JsonCoreOk(1, 2)` — the same shape Scala's case-class toString gives, so a
            // `println` of a value that happens to live in an `Any` reads the same as one
            // that does not.
            Value::Map(kvs) => {
                let parts: Vec<String> =
                    kvs.iter().map(|(k, v)| format!("{} -> {}", k, v.show())).collect();
                format!("Map({})", parts.join(", "))
            }
            Value::Obj(name, fs) => {
                // `None` is a case OBJECT and Scala prints it without parens; a field-less
                // case CLASS keeps them, as `case class Marker()` does.
                if *name == "None" { "None".to_string() }
                else if fs.is_empty() { format!("{}()", name) }
                else { format!("{}{}", name, render_seq("(", ")", fs)) }
            }
        }
    }
    // Unwrap a signal to its current value (or itself if not a signal). Takes
    // `&self` + clones so a signal read `loc.signal_value()` inside a (possibly
    // repeatedly-called) computed-signal closure doesn't move the captured value.
    // LIVE: prefer the server-side store (updated by push + recompute) over the
    // inline SSR value, so a re-run of a computed closure sees new dependency values.
    #[allow(dead_code)]
    pub fn signal_value(&self) -> Value {
        match self {
            Value::Signal(name, v) => {
                if !name.is_empty() {
                    if let Some(s) = ssc_signals().lock().unwrap().get(name) {
                        return Value::Str(s.clone());
                    }
                }
                (**v).clone()
            }
            other => other.clone(),
        }
    }
}

// Lets a `Value` be formatted with `{}` (e.g. by the std/ui `_ui_attr` coercion),
// reusing the same rendering as `.show()`.
impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.show())
    }
}

impl Value {
    // Truthiness for `showSignal(cond, …)` SSR: only `false`/`Unit` are falsey.
    // A signal unwraps to its current value.
    #[allow(dead_code)]
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(false) | Value::Unit => false,
            Value::Signal(_, v)              => v.is_truthy(),
            _                                => true,
        }
    }
    // Unwrap an Int (used by the multi-shot effect interpreter to thread resume values). 0 otherwise.
    #[allow(dead_code)]
    pub fn as_int(&self) -> i64 {
        match self { Value::Int(n) => *n, Value::Bool(b) => if *b { 1 } else { 0 }, _ => 0 }
    }
    // ── Dynamic accessors, so code written against `Any` reaches its contents ──────
    // A typed pattern (`case l: List[Any]`) does not narrow the arm's binding, so a body
    // that says `l.length` or `m.get(k)` emits those calls ON the `Value`. Answering them
    // here keeps the representation private: `Map` stays an ORDERED `Vec<(String, Value)>`
    // — the ordering the enum's own comment says a BTreeMap would have quietly destroyed —
    // and the linear scan lives behind the accessor instead of leaking into generated code.
    // Total, like `as_int` above: a wrong variant answers 0 / None / empty rather than
    // panicking, because these run in generated code with no place to report an error.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        match self {
            Value::List(xs)  => xs.len(),
            Value::Tuple(xs) => xs.len(),
            Value::Map(kvs)  => kvs.len(),
            Value::Str(s)    => s.chars().count(),
            Value::Obj(_, f) => f.len(),
            _                => 0,
        }
    }
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    // First value for `k`. FIRST, not last: `Map` keeps insertion order, and a duplicate
    // key in JSON input should read as the one the document introduced.
    #[allow(dead_code)]
    pub fn get(&self, k: &str) -> Option<&Value> {
        match self {
            Value::Map(kvs) => kvs.iter().find(|(key, _)| key == k).map(|(_, v)| v),
            _               => None,
        }
    }
    // Iterate the elements of a List/Tuple (a Map iterates its VALUES, matching what
    // `for (v <- someMap)` means on the other lanes). Anything else is empty, not a panic.
    #[allow(dead_code)]
    pub fn iter(&self) -> std::vec::IntoIter<&Value> {
        let items: Vec<&Value> = match self {
            Value::List(xs)  => xs.iter().collect(),
            Value::Tuple(xs) => xs.iter().collect(),
            Value::Map(kvs)  => kvs.iter().map(|(_, v)| v).collect(),
            _                => Vec::new(),
        };
        items.into_iter()
    }
}

// `From` conversions so a signal's initial value (`signal(name, default)`) of a
// primitive type can be carried as a `Value` and SSR-rendered.
impl From<String> for Value { fn from(s: String) -> Self { Value::Str(s) } }
impl From<&str>   for Value { fn from(s: &str)   -> Self { Value::Str(s.to_string()) } }
impl From<bool>   for Value { fn from(b: bool)   -> Self { Value::Bool(b) } }
impl From<i64>    for Value { fn from(n: i64)    -> Self { Value::Int(n) } }
impl From<f64>    for Value { fn from(f: f64)    -> Self { Value::Double(f) } }
// (`From<Value> for Value` is already provided by std's reflexive `impl<T> From<T> for T`.)

fn format_double(f: f64) -> String {
    if f.fract() == 0.0 && f.is_finite() {
        format!("{:.1}", f)
    } else {
        f.to_string()
    }
}

fn render_seq(open: &str, close: &str, xs: &[Value]) -> String {
    let parts: Vec<String> = xs.iter().map(|v| v.show()).collect();
    format!("{}{}{}", open, parts.join(", "), close)
}

// ── Coercion across the `Any` boundary ────────────────────────────────────────
//
// These exist so the CODE GENERATOR never has to know the type of an expression it is
// passing — only the type of the place it is passing it TO, which is always declared (a
// `case class` field, a `def` parameter, a return type). That asymmetry is the whole reason
// this design is tractable without type inference in the walker.
//
// Both directions are TOTAL:
//   * into an `Any`   — `Value::from(x)`, and `impl<T> From<T> for T` in std makes it the
//                       identity when `x` is already a `Value`;
//   * out of an `Any` — `x.ssc_int()` etc., implemented for `Value` AND for the concrete
//                       type, so the same emitted call compiles either way.
//
// Getting it wrong is a PANIC naming both types, not a silent default: a JSON parser that
// reads a string as 0 produces plausible garbage, which is the failure mode this whole
// family of bugs has been about.

// The scalar `From`s are declared further up (they predate this block, for signals); only the
// ones this boundary adds are here. A duplicate impl is a hard error, not a warning.
impl From<()> for Value { fn from(_: ()) -> Value { Value::Unit } }
// A `Map(...)` literal lowers to a `std::collections::HashMap`, so lifting one into an `Any`
// has to go through here. Keys are SORTED on the way in: a HashMap has no order at all, and
// an unordered iteration would make `jsonStringify` emit a different string on different
// runs of the same program. Sorted is not the insertion order the other lanes keep — that is
// a property of how Map literals lower on this lane, and it is at least deterministic.
impl<K: std::fmt::Display, V: Into<Value>> From<std::collections::HashMap<K, V>> for Value {
    fn from(m: std::collections::HashMap<K, V>) -> Value {
        let mut kvs: Vec<(String, Value)> =
            m.into_iter().map(|(k, v)| (k.to_string(), v.into())).collect();
        kvs.sort_by(|a, b| a.0.cmp(&b.0));
        Value::Map(kvs)
    }
}

// An `Option` field lifted into an `Any`. Represented the way Scala PRINTS it — `Some(x)` and
// `None` — rather than collapsing `None` to `Unit`, which would make `Some("x")` and `"x"`
// indistinguishable once inside an `Any`.
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(o: Option<T>) -> Value {
        match o {
            Some(v) => Value::Obj("Some", vec![v.into()]),
            None    => Value::Obj("None", Vec::new()),
        }
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(xs: Vec<T>) -> Value { Value::List(xs.into_iter().map(|x| x.into()).collect()) }
}

// A TUPLE lifts into `Value::Tuple`, which existed with nothing able to produce it from a
// Rust tuple. Found the moment enums started lifting into `Any` (rust-std-corpus-badrust-column):
// `case SelectNode(options: List[(String, String)], …)` produced `Value::from(options)`, which
// needs `(String, String): Into<Value>` through the `Vec<T>` impl above, and rustc answered
// `Value: From<(String, String)> is not satisfied`.
//
// Arities 2-4 by hand rather than a macro: those are what `renderTuple` emits, a macro here
// would be the only one in this file, and an arity nobody produces is a trait impl nobody
// calls. If a 5-tuple ever lowers, the failure is this exact error naming the arity.
impl<A: Into<Value>, B: Into<Value>> From<(A, B)> for Value {
    fn from(t: (A, B)) -> Value { Value::Tuple(vec![t.0.into(), t.1.into()]) }
}
impl<A: Into<Value>, B: Into<Value>, C: Into<Value>> From<(A, B, C)> for Value {
    fn from(t: (A, B, C)) -> Value { Value::Tuple(vec![t.0.into(), t.1.into(), t.2.into()]) }
}
impl<A: Into<Value>, B: Into<Value>, C: Into<Value>, D: Into<Value>> From<(A, B, C, D)> for Value {
    fn from(t: (A, B, C, D)) -> Value { Value::Tuple(vec![t.0.into(), t.1.into(), t.2.into(), t.3.into()]) }
}

#[allow(dead_code)]
fn ssc_wrong(want: &str, got: &Value) -> ! {
    panic!("expected {} but found {} ({})", want, match got {
        Value::Unit => "Unit", Value::Bool(_) => "Boolean", Value::Int(_) => "Int",
        Value::Double(_) => "Double", Value::Str(_) => "String", Value::Tuple(_) => "a tuple",
        Value::List(_) => "a List", Value::Signal(_, _) => "a Signal", Value::Obj(n, _) => n,
        Value::Map(_) => "a Map",
    }, got.show())
}

#[allow(dead_code)]
pub trait SscInt  { fn ssc_int(self) -> i64; }
#[allow(dead_code)]
pub trait SscF64  { fn ssc_f64(self) -> f64; }
#[allow(dead_code)]
pub trait SscBool { fn ssc_bool(self) -> bool; }
#[allow(dead_code)]
pub trait SscStr  { fn ssc_str(self) -> String; }
#[allow(dead_code)]
pub trait SscVal  { fn ssc_val(self) -> Value; }
// A LIST CROSSING INTO `Any` ARRIVES AS EITHER SHAPE, and the emitter cannot tell which:
// `f(List(1,2,3))` hands over a `Vec<i64>`, while `f(ssc_field(&x, "Node", 0))` hands over a
// `Value` that HOLDS a list. The coercion used to assume the first and emitted
// `.into_iter().map(Value::from)`, which is `error[E0599]: Value is not an iterator` for the
// second — 27 of them across five std modules, the largest single class behind the
// parenless-member refusal. Two impls make one emission serve both, which is the same
// property `ssc_int` has for scalars.
#[allow(dead_code)]
pub trait SscValList { fn ssc_val_list(self) -> Vec<Value>; }

impl SscValList for Value      { fn ssc_val_list(self) -> Vec<Value> { ssc_vec(self) } }
// `T: Into<Value>` covers `Vec<Value>` too — `From` is reflexive, so that case is the
// identity map and needs no impl of its own (which would not be allowed to exist anyway).
impl<T: Into<Value>> SscValList for Vec<T> { fn ssc_val_list(self) -> Vec<Value> { self.into_iter().map(Into::into).collect() } }
impl SscInt for Value { fn ssc_int(self) -> i64 { match self { Value::Int(n) => n, other => ssc_wrong("Int", &other) } } }
impl SscInt for i64   { fn ssc_int(self) -> i64 { self } }
impl SscF64 for Value { fn ssc_f64(self) -> f64 { match self { Value::Double(f) => f, Value::Int(n) => n as f64, other => ssc_wrong("Double", &other) } } }
impl SscF64 for f64   { fn ssc_f64(self) -> f64 { self } }
// A SIGNAL's store is String-valued (it is the `data-ssc-text` wiring), so a boolean signal
// read back is `Value::Str("true")`, not `Value::Bool`. Rejecting it would make every
// `if someBoolSignal()` panic at runtime — std/ui/state.ssc is written that way. Only the two
// exact spellings are accepted; anything else still reports its real type rather than
// guessing, so a genuine wrong-type error stays loud.
impl SscBool for Value {
    fn ssc_bool(self) -> bool {
        match self {
            Value::Bool(b) => b,
            Value::Str(ref s) if s == "true"  => true,
            Value::Str(ref s) if s == "false" => false,
            other => ssc_wrong("Boolean", &other),
        }
    }
}
impl SscBool for bool  { fn ssc_bool(self) -> bool { self } }
impl SscStr for Value  { fn ssc_str(self) -> String { match self { Value::Str(s) => s, other => ssc_wrong("String", &other) } } }
impl SscStr for String { fn ssc_str(self) -> String { self } }
impl SscStr for &str   { fn ssc_str(self) -> String { self.to_string() } }
// `ssc_val` is the identity on Value and the lift on everything else, so a call argument
// declared `Any` can be emitted the same way regardless of what it is given.
impl<T: Into<Value>> SscVal for T { fn ssc_val(self) -> Value { self.into() } }

// A TYPE TEST, total the same way the coercions above are.
//
// `case Some(s: String)` has to become a Rust match GUARD — a Rust pattern has nowhere to put
// a type test — and a guard may name the bindings its own pattern introduced. What kept that
// unfixed is that `matches!(s, Value::Str(_))` compiles only when the binder really IS a
// `Value`: for `Map[String, Any]` it is, for `List[String]` the binder is a `String` and the
// identical guard is a type error. Telling the two apart needs an element type the emitter
// does not track, and emitting the guard anyway would turn valid source into invalid Rust.
//
// One trait per tested type, implemented for `Value` (a real variant test) AND for the
// concrete type (statically true — a `String` binder IS a String), REMOVES the question
// rather than answering it: the same emitted guard is correct either way. That is exactly the
// property `SscStr`/`SscInt` above already rely on, applied to the test instead of to the
// coercion. `&self` so a binder that arrived as `&Value` — which is what `map.get(k)` hands
// back — resolves by autoderef.
//
// The tests are STRICT where the coercions are lenient, and that difference is deliberate:
// `ssc_bool` accepts `Value::Str("true")` because a signal's store is String-valued, but a
// TEST that did the same would make `case Some(s: String)` and `case Some(b: Boolean)` both
// match one value and leave arm ORDER to decide silently. A coercion is told what the value
// should be; a test is asking.
#[allow(dead_code)]
pub trait SscIsStr  { fn ssc_is_str(&self)  -> bool; }
#[allow(dead_code)]
pub trait SscIsInt  { fn ssc_is_int(&self)  -> bool; }
#[allow(dead_code)]
pub trait SscIsF64  { fn ssc_is_f64(&self)  -> bool; }
#[allow(dead_code)]
pub trait SscIsBool { fn ssc_is_bool(&self) -> bool; }
#[allow(dead_code)]
pub trait SscIsList { fn ssc_is_list(&self) -> bool; }
#[allow(dead_code)]
pub trait SscIsMap  { fn ssc_is_map(&self)  -> bool; }

impl SscIsStr for Value  { fn ssc_is_str(&self) -> bool { matches!(self, Value::Str(_)) } }
impl SscIsStr for String { fn ssc_is_str(&self) -> bool { true } }
impl SscIsStr for &str   { fn ssc_is_str(&self) -> bool { true } }
impl SscIsInt for Value  { fn ssc_is_int(&self) -> bool { matches!(self, Value::Int(_)) } }
impl SscIsInt for i64    { fn ssc_is_int(&self) -> bool { true } }
// `Int` and `Double` are separate ARMS in the extractors this exists for — `requireDouble`
// reads `case Some(n: Double)` then `case Some(n: Int) => n.toDouble` — so the Double test
// must not accept an Int, even though `ssc_f64` widens one. Accepting it would make the
// first arm win and the second unreachable.
impl SscIsF64 for Value  { fn ssc_is_f64(&self) -> bool { matches!(self, Value::Double(_)) } }
impl SscIsF64 for f64    { fn ssc_is_f64(&self) -> bool { true } }
impl SscIsBool for Value { fn ssc_is_bool(&self) -> bool { matches!(self, Value::Bool(_)) } }
impl SscIsBool for bool  { fn ssc_is_bool(&self) -> bool { true } }
impl SscIsList for Value { fn ssc_is_list(&self) -> bool { matches!(self, Value::List(_)) } }
impl<T> SscIsList for Vec<T> { fn ssc_is_list(&self) -> bool { true } }
impl SscIsMap for Value  { fn ssc_is_map(&self) -> bool { matches!(self, Value::Map(_)) } }
impl<K, V> SscIsMap for std::collections::HashMap<K, V> { fn ssc_is_map(&self) -> bool { true } }

// A `List[T]` crossing the boundary: `Vec<Value>` out of a `Value::List`, then elementwise.
#[allow(dead_code)]
pub fn ssc_vec(v: Value) -> Vec<Value> {
    match v { Value::List(xs) => xs, Value::Tuple(xs) => xs, other => ssc_wrong("a List", &other) }
}

// Read field `i` of a case-class value that is living in an `Any`. The name is checked, so a
// pattern that expects `JsonCoreOk` and meets a `JsonCoreErr` says which it got.
#[allow(dead_code)]
pub fn ssc_field(v: &Value, ctor: &str, i: usize) -> Value {
    match v {
        Value::Obj(name, fs) if *name == ctor => fs[i].clone(),
        other => ssc_wrong(ctor, other),
    }
}

// A `Value::Map` is an ASSOC LIST — insertion order is part of the contract, see the note on the
// variant — while a case-class field declared `Map[String, Any]` is a `HashMap` in emitted
// code. `ssc_vec`'s counterpart for that boundary: without it a runtime handing back a map
// could only be turned into an EMPTY one at the call site, which is a silent lie about data
// the server did send.
#[allow(dead_code)]
pub fn ssc_map(v: Value) -> std::collections::HashMap<String, Value> {
    match v {
        Value::Map(kvs) => kvs.into_iter().collect(),
        other => ssc_wrong("a Map", &other),
    }
}

#[allow(dead_code)]
pub fn ssc_is(v: &Value, ctor: &str) -> bool {
    matches!(v, Value::Obj(name, _) if *name == ctor)
}
