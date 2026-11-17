//! The JavaScript side of the `code` tool: one QuickJS context per `code`
//! tool (so per conversation or subagent), kept on a thread of its own, so
//! what one script defines at its top level is there for the next. QuickJS
//! has no file system, network or process access of its own (the `std`/`os`
//! modules are not built in); the only way out is `__send`, which hands a
//! tool call to the async side and gets its result back through [`Reply`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rquickjs::context::EvalOptions;
use rquickjs::{Context, Ctx, FromJs, Function, Object, Promise, Runtime, Value, promise::PromiseState};
use serde_json::Value as Json;

/// What the scripts of one context may allocate, together.
const MEMORY_LIMIT: usize = 256 << 20;
/// Well below the stack of the context's thread ([`THREAD_STACK`]).
const STACK_LIMIT: usize = 1 << 20;
const THREAD_STACK: usize = 4 << 20;
/// What `print` may collect in one script before it is stopped.
const PRINT_LIMIT: usize = 16 << 20;

/// A tool call the script made.
pub(super) struct Call {
    pub id: u64,
    pub name: String,
    pub input: Json,
}

/// The result of the call `id`: its text, or the error it gave. With
/// `json`, the text is JSON the script gets parsed.
pub(super) struct Reply {
    pub id: u64,
    pub result: Result<String, String>,
    pub json: bool,
}

/// How a script ended.
#[derive(Debug, PartialEq)]
pub(super) enum End {
    /// It returned (or ran to its end).
    Done,
    /// It threw, or its promise was rejected, with this message.
    Threw(String),
    /// It was stopped from outside: timed out, cancelled or halted.
    Stopped,
}

/// The tools the script can call: name, description and input schema.
pub(super) struct Callable {
    pub name: String,
    pub description: String,
    pub schema: Json,
}

/// One script to run. Its tool calls go to `calls`; their results come back
/// through `replies`, in any order. `stop` set, or `replies` closed, ends
/// it. What it printed (and returned) and how it ended go to `done`.
pub(super) struct Job {
    pub code: String,
    pub calls: tokio::sync::mpsc::UnboundedSender<Call>,
    pub replies: std::sync::mpsc::Receiver<Reply>,
    pub stop: Arc<AtomicBool>,
    pub done: tokio::sync::oneshot::Sender<(String, End)>,
}

/// The context scripts run in, on a thread started with the first script
/// and ended when this is dropped. Scripts run one at a time, in the order
/// they come.
pub(super) struct Session {
    tools: Arc<[Callable]>,
    jobs: Mutex<Option<std::sync::mpsc::Sender<Job>>>,
}

impl Session {
    pub fn new(tools: Vec<Callable>) -> Self {
        Self { tools: tools.into(), jobs: Mutex::new(None) }
    }

    /// Queues `job`, starting the thread if there is none (or it died).
    pub fn run(&self, job: Job) {
        let mut jobs = self.jobs.lock().unwrap();
        let job = match jobs.as_ref() {
            Some(tx) => match tx.send(job) {
                Ok(()) => return,
                Err(std::sync::mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let tools = self.tools.clone();
        let thread = std::thread::Builder::new().name("code".into()).stack_size(THREAD_STACK);
        let started = thread.spawn(move || serve(&tools, rx));
        if let Err(e) = started {
            let _ = job.done.send((String::new(), End::Threw(format!("could not start the script's thread: {e}"))));
            return;
        }
        let _ = tx.send(job);
        *jobs = Some(tx);
    }
}

/// Runs the jobs that come on `jobs` in one context, made with the first.
/// A context that fails inside (not a script's own error) is dropped, and
/// the next script gets a new one.
fn serve(tools: &[Callable], jobs: std::sync::mpsc::Receiver<Job>) {
    let mut state: Option<State> = None;
    for job in jobs {
        if job.stop.load(Ordering::Relaxed) {
            let _ = job.done.send((String::new(), End::Stopped));
            continue;
        }
        if state.is_none() {
            match State::new(tools) {
                Ok(new) => state = Some(new),
                Err(e) => {
                    let _ = job.done.send((String::new(), End::Threw(format!("internal error: {e}"))));
                    continue;
                }
            }
        }
        let current = state.as_ref().expect("made above");
        let (text, end) = match current.run(&job.code, job.calls, &job.replies, &job.stop) {
            Ok(done) => done,
            Err((text, e)) => {
                state = None;
                let reset = "The context was reset: what earlier scripts defined is gone";
                (text, End::Threw(format!("internal error: {e}. {reset}")))
            }
        };
        let _ = job.done.send((text, end));
    }
}

/// A context and what its globals write to.
struct State {
    // Dropped before the runtime.
    context: Context,
    _runtime: Runtime,
    /// The running script's stop flag, for the interrupt handler.
    stop: Arc<Mutex<Arc<AtomicBool>>>,
    /// Where the running script's tool calls go; none between scripts, so
    /// code a finished script left behind calls nothing.
    calls: Rc<RefCell<Option<tokio::sync::mpsc::UnboundedSender<Call>>>>,
    /// Calls sent and not yet answered.
    outstanding: Rc<Cell<usize>>,
    printed: Rc<RefCell<String>>,
}

impl State {
    fn new(tools: &[Callable]) -> rquickjs::Result<Self> {
        let runtime = Runtime::new()?;
        runtime.set_memory_limit(MEMORY_LIMIT);
        runtime.set_max_stack_size(STACK_LIMIT);
        let stop = Arc::new(Mutex::new(Arc::new(AtomicBool::new(false))));
        let flag = stop.clone();
        runtime.set_interrupt_handler(Some(Box::new(move || flag.lock().unwrap().load(Ordering::Relaxed))));
        let context = Context::full(&runtime)?;
        let state = Self {
            context,
            _runtime: runtime,
            stop,
            calls: Rc::default(),
            outstanding: Rc::default(),
            printed: Rc::default(),
        };
        let (calls, outstanding, printed) = (state.calls.clone(), state.outstanding.clone(), state.printed.clone());
        state.context.with(|ctx| install(&ctx, tools, calls, outstanding, printed))?;
        Ok(state)
    }

    /// Runs `code` to its end. `Err` carries what it printed and how the
    /// context failed.
    fn run(
        &self,
        code: &str,
        calls: tokio::sync::mpsc::UnboundedSender<Call>,
        replies: &std::sync::mpsc::Receiver<Reply>,
        stop: &Arc<AtomicBool>,
    ) -> Result<(String, End), (String, rquickjs::Error)> {
        *self.stop.lock().unwrap() = stop.clone();
        self.printed.borrow_mut().clear();
        self.outstanding.set(0);
        *self.calls.borrow_mut() = Some(calls);
        let end = self.context.with(|ctx| execute(&ctx, code, replies, stop, &self.outstanding, &self.printed));
        *self.calls.borrow_mut() = None;
        let text = std::mem::take(&mut *self.printed.borrow_mut());
        match end {
            Ok(end) => Ok((text, end)),
            Err(e) => Err((text, e)),
        }
    }
}

fn execute(
    ctx: &Ctx<'_>,
    code: &str,
    replies: &std::sync::mpsc::Receiver<Reply>,
    stop: &AtomicBool,
    outstanding: &Cell<usize>,
    printed: &RefCell<String>,
) -> rquickjs::Result<End> {
    let forget: Function = ctx.globals().get("__forget")?;
    forget.call::<_, ()>(())?;
    // A top-level script, so its declarations outlive it, with top-level
    // `await`; it settles to `{value}`, its last expression's value. One
    // that `return`s runs as an async function body instead, as scripts
    // were written before they shared a context.
    let mut options = EvalOptions::default();
    options.global = true;
    options.promise = true;
    options.strict = false;
    let (mut promise, mut completion): (Promise, bool) = match ctx.eval_with_options(lexical_to_var(code), options) {
        Ok(promise) => (promise, true),
        Err(rquickjs::Error::Exception) => {
            let error = ctx.catch();
            if !describe(&error).contains("return not in a function") {
                return Ok(threw_with(error, stop));
            }
            // On the first line, so error line numbers are the script's own.
            match ctx.eval(format!("(async () => {{{code}\n}})()")) {
                Ok(promise) => (promise, false),
                Err(rquickjs::Error::Exception) => return Ok(threw(ctx, stop)),
                Err(e) => return Err(e),
            }
        }
        Err(e) => return Err(e),
    };
    let settle: Function = ctx.globals().get("__settle")?;
    loop {
        while ctx.execute_pending_job() {}
        if stop.load(Ordering::Relaxed) {
            return Ok(End::Stopped);
        }
        match promise.state() {
            PromiseState::Resolved => {
                let mut value: Value = promise.result().unwrap_or(Ok(Value::new_undefined(ctx.clone())))?;
                if completion {
                    value = value.as_object().map_or(Ok(Value::new_undefined(ctx.clone())), |o| o.get("value"))?;
                    // A promise left last (a call not awaited) is awaited.
                    if value.is_promise() {
                        (promise, completion) = (Promise::from_js(ctx, value)?, false);
                        continue;
                    }
                }
                append(&mut printed.borrow_mut(), &show(ctx, value)?);
                let unawaited = outstanding.get() - replies.try_iter().count();
                if unawaited > 0 {
                    let note = format!("({unawaited} tool call(s) the script did not await were cancelled.)");
                    append(&mut printed.borrow_mut(), &note);
                }
                return Ok(End::Done);
            }
            PromiseState::Rejected => {
                let _ = promise.result::<Value>();
                return Ok(threw(ctx, stop));
            }
            PromiseState::Pending => {}
        }
        if outstanding.get() == 0 {
            return Ok(End::Threw(
                "the script is waiting on a promise that nothing will settle (no tool call is running)".into(),
            ));
        }
        let Ok(reply) = replies.recv() else { return Ok(End::Stopped) };
        outstanding.set(outstanding.get() - 1);
        let (ok, text) = match reply.result {
            Ok(text) => (true, text),
            Err(text) => (false, text),
        };
        // An exception here is the script's own (a throwing callback);
        // it surfaces through the main promise.
        match settle.call::<_, ()>((reply.id as f64, ok, text, reply.json)) {
            Ok(()) | Err(rquickjs::Error::Exception) => {}
            Err(e) => return Err(e),
        }
    }
}

/// The globals the scripts see: `tools`, `ALL_TOOLS`, `print` and `console`.
fn install<'js>(
    ctx: &Ctx<'js>,
    tools: &[Callable],
    calls: Rc<RefCell<Option<tokio::sync::mpsc::UnboundedSender<Call>>>>,
    outstanding: Rc<Cell<usize>>,
    printed: Rc<RefCell<String>>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let next = Cell::new(0u64);
    let send = Function::new(ctx.clone(), move |ctx: Ctx<'js>, name: String, input: String| -> rquickjs::Result<f64> {
        let input = serde_json::from_str(&input).unwrap_or(Json::Null);
        let id = next.get() + 1;
        next.set(id);
        let calls = calls.borrow();
        let Some(calls) = calls.as_ref() else {
            let message = "this call was not made: the script that started this code has ended, and code it left \
                           running cannot call tools. Await each call inside the script.";
            return Err(rquickjs::Exception::throw_message(&ctx, message));
        };
        if calls.send(Call { id, name, input }).is_err() {
            return Err(rquickjs::Exception::throw_message(&ctx, "the script is being stopped"));
        }
        outstanding.set(outstanding.get() + 1);
        Ok(id as f64)
    })?;
    globals.set("__send", send)?;
    let print = Function::new(ctx.clone(), move |ctx: Ctx<'js>, text: String| -> rquickjs::Result<()> {
        let mut printed = printed.borrow_mut();
        if printed.len() + text.len() > PRINT_LIMIT {
            return Err(rquickjs::Exception::throw_message(&ctx, "the script printed more than 16 MB"));
        }
        printed.push_str(&text);
        printed.push('\n');
        Ok(())
    })?;
    globals.set("__print", print)?;
    let listed: Vec<Json> = tools
        .iter()
        .map(|t| serde_json::json!({"name": t.name, "description": t.description, "input_schema": t.schema}))
        .collect();
    globals.set("__tools", ctx.json_parse(Json::Array(listed).to_string())?)?;
    ctx.eval::<(), _>(PRELUDE)?;
    let _: Object = globals.get("tools")?;
    Ok(())
}

/// Built on `__send`, `__print` and `__tools`, which it then hides. What
/// it defines cannot be replaced by a script, so it stays for the next.
const PRELUDE: &str = r#"
(() => {
  const send = globalThis.__send, write = globalThis.__print, listed = globalThis.__tools;
  delete globalThis.__send; delete globalThis.__print; delete globalThis.__tools;
  const fixed = (name, value) => Object.defineProperty(globalThis, name, { value, enumerable: false });
  let pending = new Map();
  fixed("__settle", (id, ok, text, json) => {
    const p = pending.get(id);
    if (!p) return;
    pending.delete(id);
    if (ok) p.resolve(json ? JSON.parse(text) : text); else p.reject(new Error(text));
  });
  // A new script: calls an earlier one left unanswered never will be.
  fixed("__forget", () => { pending = new Map(); });
  const names = new Set(listed.map((t) => t.name));
  const call = (name, input) => new Promise((resolve, reject) => {
    const id = send(name, JSON.stringify(input === undefined ? {} : input));
    pending.set(id, { resolve, reject });
  });
  const table = {};
  for (const name of names) table[name] = (input) => call(name, input);
  fixed("tools", new Proxy(Object.freeze(table), {
    get(target, name) {
      if (typeof name !== "string" || name in target) return target[name];
      if (name === "then" || name === "toJSON") return undefined;
      throw new TypeError(`no tool named ${name}; ALL_TOOLS lists the ones there are`);
    },
  }));
  fixed("ALL_TOOLS", Object.freeze(listed.map((t) => Object.freeze(t))));
  const show = (v) => typeof v === "string" ? v : (() => {
    try { const s = JSON.stringify(v, null, 2); return s === undefined ? String(v) : s; } catch { return String(v); }
  })();
  const print = (...values) => write(values.map(show).join(" "));
  fixed("print", print);
  fixed("console", Object.freeze({ log: print, info: print, warn: print, error: print, debug: print }));
})();
"#;

/// `code` with its top-level `let` and `const` declarations made `var`, so
/// that a later script can declare the same name again (a lexical one
/// would be a redeclaration error) and assign what was a constant. Only
/// declarations outside any bracket, string, template, comment or regular
/// expression change; the rest is kept byte for byte, lines included.
fn lexical_to_var(code: &str) -> String {
    let bytes = code.as_bytes();
    let mut out = String::with_capacity(code.len());
    // Brackets open, and for each template `${` the depth it started at.
    let mut depth = 0usize;
    let mut templates: Vec<usize> = Vec::new();
    // Whether a `/` here starts a regular expression rather than divides.
    let mut regex_ok = true;
    let mut i = 0;
    let mut copied = 0;
    // Scans a template's text from `i` (after "`" or "}") to its end or to
    // a `${`, whose depth is pushed.
    let template = |i: &mut usize, depth: usize, templates: &mut Vec<usize>| {
        while *i < bytes.len() {
            match bytes[*i] {
                b'\\' => *i += 2,
                b'`' => {
                    *i += 1;
                    return;
                }
                b'$' if bytes.get(*i + 1) == Some(&b'{') => {
                    *i += 2;
                    templates.push(depth);
                    return;
                }
                _ => *i += 1,
            }
        }
    };
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i = code[i + 2..].find("*/").map_or(bytes.len(), |end| i + 2 + end + 2);
            }
            b'/' if regex_ok => {
                i += 1;
                let mut class = false;
                while i < bytes.len() && bytes[i] != b'\n' {
                    match bytes[i] {
                        b'\\' => i += 1,
                        b'[' => class = true,
                        b']' => class = false,
                        b'/' if !class => break,
                        _ => {}
                    }
                    i += 1;
                }
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                    i += 1;
                }
                regex_ok = false;
            }
            b'\'' | b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != c && bytes[i] != b'\n' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
                regex_ok = false;
            }
            b'`' => {
                i += 1;
                template(&mut i, depth, &mut templates);
                regex_ok = false;
            }
            b'}' if templates.last() == Some(&depth) => {
                templates.pop();
                i += 1;
                template(&mut i, depth, &mut templates);
                regex_ok = false;
            }
            b'(' | b'[' | b'{' => {
                depth += 1;
                i += 1;
                regex_ok = true;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                i += 1;
                // After `}` a regular expression is likelier than a division.
                regex_ok = c == b'}';
            }
            c if c == b'_' || c == b'$' || c.is_ascii_alphabetic() || c >= 0x80 => {
                let start = i;
                let word_byte = |b: u8| b == b'_' || b == b'$' || b.is_ascii_alphanumeric() || b >= 0x80;
                while i < bytes.len() && word_byte(bytes[i]) {
                    i += 1;
                }
                let word = &code[start..i];
                let after = code[i..].trim_start_matches([' ', '\t']);
                let declares = after.starts_with(|c: char| matches!(c, '{' | '[' | '_' | '$') || c.is_alphabetic());
                let property = code[..start].trim_end().ends_with('.');
                if depth == 0 && (word == "let" || word == "const") && declares && !property {
                    out.push_str(&code[copied..start]);
                    out.push_str("var");
                    copied = i;
                }
                const BEFORE_EXPRESSION: [&str; 14] = [
                    "return", "typeof", "instanceof", "in", "of", "new", "delete", "void", "throw", "case", "do", "else",
                    "await", "yield",
                ];
                regex_ok = BEFORE_EXPRESSION.contains(&word);
            }
            _ => {
                i += 1;
                regex_ok = !c.is_ascii_digit() && c != b'.';
            }
        }
    }
    out.push_str(&code[copied.min(code.len())..]);
    out
}

/// The text of a returned value: a string as it is, anything else as JSON;
/// nothing for `undefined`.
fn show<'js>(ctx: &Ctx<'js>, value: Value<'js>) -> rquickjs::Result<String> {
    if value.is_undefined() {
        return Ok(String::new());
    }
    if let Some(text) = value.as_string() {
        return text.to_string();
    }
    let json: Option<rquickjs::String> = ctx.json_stringify_replacer_space(value.clone(), Value::new_undefined(ctx.clone()), 2)?;
    match json {
        Some(json) => json.to_string(),
        None => Ok(String::new()),
    }
}

fn append(printed: &mut String, text: &str) {
    if !text.is_empty() {
        printed.push_str(text);
        printed.push('\n');
    }
}

/// How the script ended after an exception is pending: stopped, if that is
/// what interrupted it, else its message and stack.
fn threw(ctx: &Ctx<'_>, stop: &AtomicBool) -> End {
    threw_with(ctx.catch(), stop)
}

fn threw_with(error: Value<'_>, stop: &AtomicBool) -> End {
    if stop.load(Ordering::Relaxed) {
        return End::Stopped;
    }
    End::Threw(describe(&error))
}

fn describe(error: &Value<'_>) -> String {
    if let Some(exception) = error.as_exception() {
        let message = exception.message().unwrap_or_default();
        let stack = exception.stack().unwrap_or_default();
        let name: String = exception.get("name").unwrap_or_else(|_| "Error".into());
        let head = if message.is_empty() { name } else { format!("{name}: {message}") };
        let stack = stack.trim_end();
        return if stack.is_empty() { head } else { format!("{head}\n{stack}") };
    }
    if let Some(text) = error.as_string().and_then(|s| s.to_string().ok()) {
        return text;
    }
    format!("{error:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools() -> Vec<Callable> {
        vec![
            Callable { name: "echo".into(), description: "echoes".into(), schema: serde_json::json!({"type": "object"}) },
            Callable { name: "json".into(), description: "answers JSON".into(), schema: serde_json::json!({}) },
            Callable { name: "mcp__a-b__c".into(), description: "dashed".into(), schema: serde_json::json!({}) },
        ]
    }

    /// Runs `code` in `session`, answering each call with `answer`.
    fn run_in(session: &Session, code: &str, answer: impl Fn(&Call) -> Result<String, String>) -> (String, End, Vec<String>) {
        let (call_tx, mut call_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        session.run(Job { code: code.into(), calls: call_tx, replies: reply_rx, stop, done: done_tx });
        let mut seen = Vec::new();
        while let Some(call) = call_rx.blocking_recv() {
            seen.push(format!("{}:{}", call.name, call.input));
            let json = call.name == "json";
            let _ = reply_tx.send(Reply { id: call.id, result: answer(&call), json });
        }
        let (text, end) = loop {
            match done_rx.try_recv() {
                Ok(done) => break done,
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => std::thread::yield_now(),
                Err(e) => panic!("{e}"),
            }
        };
        (text, end, seen)
    }

    /// Runs `code` in a context of its own.
    fn script(code: &str, answer: impl Fn(&Call) -> Result<String, String>) -> (String, End, Vec<String>) {
        run_in(&Session::new(tools()), code, answer)
    }

    #[test]
    fn prints_and_gives_the_last_value_or_what_is_returned() {
        let (text, end, _) = script("print('a', 1, {b: 2}); console.log('c'); ({d: [1]})", |_| Ok(String::new()));
        assert_eq!(end, End::Done);
        assert_eq!(text, "a 1 {\n  \"b\": 2\n}\nc\n{\n  \"d\": [\n    1\n  ]\n}\n");
        // As an async function body, as scripts were written before.
        let (text, end, _) = script("print('a'); return {d: [1]};", |_| Ok(String::new()));
        assert_eq!((text.as_str(), end), ("a\n{\n  \"d\": [\n    1\n  ]\n}\n", End::Done));
        let (text, _, _) = script("const x = 1;", |_| Ok(String::new()));
        assert_eq!(text, "");
    }

    #[test]
    fn what_a_script_defines_stays_for_the_next() {
        let session = Session::new(tools());
        let answer = |call: &Call| Ok(format!("got {}", call.input));
        let (_, end, _) = run_in(&session, "const out = await tools.echo({n: 1});\nlet n = 2; function f() { return n }", answer);
        assert_eq!(end, End::Done);
        let (text, end, _) = run_in(&session, "out + ' ' + f()", answer);
        assert_eq!((text.as_str(), end), ("got {\"n\":1} 2\n", End::Done));
        // Declared again, as a model keeping results in variables does.
        let (text, end, _) = run_in(&session, "const out = 'again'; let n = 3; out + n", answer);
        assert_eq!((text.as_str(), end), ("again3\n", End::Done));
        // What the harness defines stays.
        let (text, end, _) = run_in(&session, "var print = 1; tools = 2; typeof print + ' ' + typeof tools", answer);
        assert_eq!((text.as_str(), end), ("function object\n", End::Done));
        // Another session has a context of its own.
        let (_, end, _) = script("out", answer);
        assert!(matches!(&end, End::Threw(m) if m.contains("out is not defined")), "{end:?}");
    }

    #[test]
    fn only_top_level_declarations_become_var() {
        let cases = [
            ("const a = 1", "var a = 1"),
            ("let {a, b} = o;\nconst [c] = d", "var {a, b} = o;\nvar [c] = d"),
            ("for (const x of y) { let z = x }", "for (const x of y) { let z = x }"),
            ("if (a) { const b = 1 }\nconst c = 2", "if (a) { const b = 1 }\nvar c = 2"),
            ("const s = 'const x = 1'; `let ${ {a: 1}.a } const y`", "var s = 'const x = 1'; `let ${ {a: 1}.a } const y`"),
            ("// const a\n/* let b */ const c = /const d/g.test(e) / 2", "// const a\n/* let b */ var c = /const d/g.test(e) / 2"),
            ("x = a / 2; const b = 1 // let c", "x = a / 2; var b = 1 // let c"),
            ("o.let = 1; let = 2; constant = 3", "o.let = 1; let = 2; constant = 3"),
            ("const t = `a ${`b ${c}`} d`; const e = 1", "var t = `a ${`b ${c}`} d`; var e = 1"),
        ];
        for (code, expected) in cases {
            assert_eq!(lexical_to_var(code), expected, "{code}");
        }
    }

    #[test]
    fn calls_tools_concurrently_and_gets_their_text() {
        let code = r#"
            const [a, b] = await Promise.all([tools.echo({x: 1}), tools["mcp__a-b__c"]({y: "z"})]);
            a + "|" + b
        "#;
        let (text, end, seen) = script(code, |call| Ok(format!("got {}", call.input)));
        assert_eq!(end, End::Done);
        assert_eq!(text.trim(), r#"got {"x":1}|got {"y":"z"}"#);
        assert_eq!(seen, [r#"echo:{"x":1}"#, r#"mcp__a-b__c:{"y":"z"}"#]);
    }

    #[test]
    fn a_failed_call_rejects_and_can_be_caught() {
        let code = r#"
            try { await tools.echo(); } catch (e) { print("caught: " + e.message); }
            await tools.echo({});
        "#;
        let (text, end, _) = script(code, |_| Err("nope".into()));
        assert_eq!(text, "caught: nope\n");
        assert!(matches!(&end, End::Threw(m) if m.starts_with("Error: nope")), "{end:?}");
    }

    #[test]
    fn errors_name_unknown_tools_and_syntax() {
        let (_, end, _) = script("await tools.nope({})", |_| Ok(String::new()));
        assert!(matches!(&end, End::Threw(m) if m.contains("no tool named nope")), "{end:?}");
        let (_, end, _) = script("let = ;", |_| Ok(String::new()));
        assert!(matches!(&end, End::Threw(m) if m.starts_with("SyntaxError")), "{end:?}");
        let (_, end, _) = script("await new Promise(() => {})", |_| Ok(String::new()));
        assert!(matches!(&end, End::Threw(m) if m.contains("nothing will settle")), "{end:?}");
    }

    #[test]
    fn lists_tools_and_has_no_host_access() {
        let code = r#"
            [ALL_TOOLS.map(t => t.name).join(","), typeof require, typeof process, typeof fetch,
             typeof globalThis.__send, typeof __settle]
        "#;
        let (text, end, _) = script(code, |_| Ok(String::new()));
        assert_eq!(end, End::Done);
        let shown: Json = serde_json::from_str(&text).unwrap();
        let expected = ["echo,json,mcp__a-b__c", "undefined", "undefined", "undefined", "undefined", "function"];
        assert_eq!(shown, serde_json::json!(expected));
    }

    #[test]
    fn a_call_with_a_value_resolves_to_it() {
        let (text, end, _) = script("const files = await tools.json({}); [files.length, files[1]]", |_| Ok(r#"["a","b"]"#.into()));
        assert_eq!(end, End::Done);
        assert_eq!(serde_json::from_str::<Json>(&text).unwrap(), serde_json::json!([2, "b"]));
    }

    #[test]
    fn says_which_calls_were_not_awaited_and_numbers_lines_as_written() {
        let slow = |_: &Call| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(String::new())
        };
        let (text, end, _) = script("tools.echo({}); 'done'", slow);
        assert_eq!(end, End::Done);
        assert_eq!(text, "done\n(1 tool call(s) the script did not await were cancelled.)\n");
        let (_, end, _) = script("const a = 1;\nnull.x;", |_| Ok(String::new()));
        assert!(matches!(&end, End::Threw(m) if m.contains(":2:")), "{end:?}");
        let (_, end, _) = script("const a = 1;\nnull.x;\nreturn a", |_| Ok(String::new()));
        assert!(matches!(&end, End::Threw(m) if m.contains(":2:")), "{end:?}");
    }

    #[test]
    fn functions_kept_call_tools_and_a_promise_left_last_is_awaited() {
        let session = Session::new(tools());
        let (_, end, _) = run_in(&session, "var later = () => tools.echo({});", |_| Ok(String::new()));
        assert_eq!(end, End::Done);
        let (text, end, seen) = run_in(&session, "later()", |_| Ok("ran".into()));
        assert_eq!((text.as_str(), end, seen.len()), ("ran\n", End::Done, 1));
        let (text, end, _) = run_in(&session, "Promise.resolve().then(() => 1)", |_| Ok(String::new()));
        assert_eq!((text.as_str(), end), ("1\n", End::Done));
    }

    #[test]
    fn runaway_recursion_and_allocation_are_script_errors_and_the_context_goes_on() {
        let session = Session::new(tools());
        let (_, end, _) = run_in(&session, "var kept = 1; const f = (n) => f(n + 1) + 1; f(0);", |_| Ok(String::new()));
        assert!(matches!(&end, End::Threw(m) if m.contains("stack")), "{end:?}");
        let (_, end, _) = run_in(&session, "const a = []; for (;;) a.push(new Array(1e6).fill(1));", |_| Ok(String::new()));
        assert!(matches!(&end, End::Threw(m) if m.contains("memory")), "{end:?}");
        let (text, end, _) = run_in(&session, "kept", |_| Ok(String::new()));
        assert_eq!((text.as_str(), end), ("1\n", End::Done));
    }

    #[test]
    fn stop_interrupts_a_busy_script_and_the_next_one_runs() {
        let session = Session::new(tools());
        let (call_tx, _call_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_reply_tx, reply_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let code = "print('before'); for (;;) {}".into();
        let job = Job { code, calls: call_tx, replies: reply_rx, stop: stop.clone(), done: done_tx };
        session.run(job);
        std::thread::sleep(std::time::Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        let (text, end) = done_rx.blocking_recv().unwrap();
        assert_eq!((text.as_str(), end), ("before\n", End::Stopped));
        let (text, end, _) = run_in(&session, "'after'", |_| Ok(String::new()));
        assert_eq!((text.as_str(), end), ("after\n", End::Done));
    }

    #[test]
    fn a_closed_reply_channel_stops_a_waiting_script() {
        let session = Session::new(tools());
        let (call_tx, mut call_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Reply>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let stop = Arc::new(AtomicBool::new(false));
        session.run(Job { code: "await tools.echo({})".into(), calls: call_tx, replies: reply_rx, stop, done: done_tx });
        assert!(call_rx.blocking_recv().is_some());
        drop(reply_tx);
        assert_eq!(done_rx.blocking_recv().unwrap().1, End::Stopped);
    }
}
