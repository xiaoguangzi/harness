//! The JavaScript side of the `code` tool: one QuickJS runtime per call, on
//! a blocking thread. QuickJS has no file system, network or process access
//! of its own (the `std`/`os` modules are not built in); the only way out is
//! `__send`, which hands a tool call to the async side and gets its result
//! back through [`Reply`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use rquickjs::{Context, Ctx, Function, Object, Promise, Runtime, Value, promise::PromiseState};
use serde_json::Value as Json;

/// What the script may allocate.
const MEMORY_LIMIT: usize = 256 << 20;
/// Below the 2 MiB stack of tokio's blocking threads.
const STACK_LIMIT: usize = 1 << 20;
/// What `print` may collect before the script is stopped.
const PRINT_LIMIT: usize = 16 << 20;

/// A tool call the script made.
pub(super) struct Call {
    pub id: u64,
    pub name: String,
    pub input: Json,
}

/// The result of the call `id`: its text, or the error it gave.
pub(super) struct Reply {
    pub id: u64,
    pub result: Result<String, String>,
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

/// Runs `code` (an async function body) to its end. Each tool call goes to
/// `calls`; results come back through `replies`, in any order. `stop` set,
/// or `replies` closed, ends it. Returns what it printed (and returned) and
/// how it ended.
pub(super) fn run(
    code: &str,
    tools: &[Callable],
    calls: tokio::sync::mpsc::UnboundedSender<Call>,
    replies: std::sync::mpsc::Receiver<Reply>,
    stop: Arc<AtomicBool>,
) -> (String, End) {
    let printed = Rc::new(RefCell::new(String::new()));
    let end = match execute(code, tools, calls, replies, stop, printed.clone()) {
        Ok(end) => end,
        Err(e) => End::Threw(format!("internal error: {e}")),
    };
    let text = printed.borrow().clone();
    (text, end)
}

fn execute(
    code: &str,
    tools: &[Callable],
    calls: tokio::sync::mpsc::UnboundedSender<Call>,
    replies: std::sync::mpsc::Receiver<Reply>,
    stop: Arc<AtomicBool>,
    printed: Rc<RefCell<String>>,
) -> rquickjs::Result<End> {
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    runtime.set_max_stack_size(STACK_LIMIT);
    let interrupted = stop.clone();
    runtime.set_interrupt_handler(Some(Box::new(move || interrupted.load(Ordering::Relaxed))));
    let context = Context::full(&runtime)?;
    context.with(|ctx| {
        let outstanding = Rc::new(Cell::new(0usize));
        install(&ctx, tools, calls, outstanding.clone(), printed.clone())?;
        // The body runs as an async function, so it may `await` and `return`.
        // On the first line, so error line numbers are the script's own.
        let source = format!("(async () => {{{code}\n}})()");
        let promise: Promise = match ctx.eval(source) {
            Ok(promise) => promise,
            Err(rquickjs::Error::Exception) => return Ok(threw(&ctx, &stop)),
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
                    let value: Value = promise.result().unwrap_or(Ok(Value::new_undefined(ctx.clone())))?;
                    append(&mut printed.borrow_mut(), &show(&ctx, value)?);
                    let unawaited = outstanding.get() - replies.try_iter().count();
                    if unawaited > 0 {
                        let note = format!("({unawaited} tool call(s) the script did not await were cancelled.)");
                        append(&mut printed.borrow_mut(), &note);
                    }
                    return Ok(End::Done);
                }
                PromiseState::Rejected => {
                    let _ = promise.result::<Value>();
                    return Ok(threw(&ctx, &stop));
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
            match settle.call::<_, ()>((reply.id as f64, ok, text)) {
                Ok(()) | Err(rquickjs::Error::Exception) => {}
                Err(e) => return Err(e),
            }
        }
    })
}

/// The globals the script sees: `tools`, `ALL_TOOLS`, `print` and `console`.
fn install<'js>(
    ctx: &Ctx<'js>,
    tools: &[Callable],
    calls: tokio::sync::mpsc::UnboundedSender<Call>,
    outstanding: Rc<Cell<usize>>,
    printed: Rc<RefCell<String>>,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();
    let next = Cell::new(0u64);
    let send = Function::new(ctx.clone(), move |ctx: Ctx<'js>, name: String, input: String| -> rquickjs::Result<f64> {
        let input = serde_json::from_str(&input).unwrap_or(Json::Null);
        let id = next.get() + 1;
        next.set(id);
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

/// Built on `__send`, `__print` and `__tools`, which it then hides.
const PRELUDE: &str = r#"
(() => {
  const send = globalThis.__send, write = globalThis.__print, listed = globalThis.__tools;
  delete globalThis.__send; delete globalThis.__print; delete globalThis.__tools;
  const pending = new Map();
  Object.defineProperty(globalThis, "__settle", {
    value: (id, ok, text) => {
      const p = pending.get(id);
      if (!p) return;
      pending.delete(id);
      if (ok) p.resolve(text); else p.reject(new Error(text));
    },
  });
  const names = new Set(listed.map((t) => t.name));
  const call = (name, input) => new Promise((resolve, reject) => {
    const id = send(name, JSON.stringify(input === undefined ? {} : input));
    pending.set(id, { resolve, reject });
  });
  const table = {};
  for (const name of names) table[name] = (input) => call(name, input);
  globalThis.tools = new Proxy(Object.freeze(table), {
    get(target, name) {
      if (typeof name !== "string" || name in target) return target[name];
      if (name === "then" || name === "toJSON") return undefined;
      throw new TypeError(`no tool named ${name}; ALL_TOOLS lists the ones there are`);
    },
  });
  globalThis.ALL_TOOLS = Object.freeze(listed.map((t) => Object.freeze(t)));
  const show = (v) => typeof v === "string" ? v : (() => {
    try { const s = JSON.stringify(v, null, 2); return s === undefined ? String(v) : s; } catch { return String(v); }
  })();
  globalThis.print = (...values) => write(values.map(show).join(" "));
  globalThis.console = Object.freeze({ log: print, info: print, warn: print, error: print, debug: print });
})();
"#;

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
    let error = ctx.catch();
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

    /// Runs `code` answering each call with `answer`, on this thread.
    fn script(code: &str, answer: impl Fn(&Call) -> Result<String, String>) -> (String, End, Vec<String>) {
        let tools = vec![
            Callable { name: "echo".into(), description: "echoes".into(), schema: serde_json::json!({"type": "object"}) },
            Callable { name: "mcp__a-b__c".into(), description: "dashed".into(), schema: serde_json::json!({}) },
        ];
        let (call_tx, mut call_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let code = code.to_string();
        let engine = std::thread::spawn(move || run(&code, &tools, call_tx, reply_rx, stop));
        let mut seen = Vec::new();
        while let Some(call) = call_rx.blocking_recv() {
            seen.push(format!("{}:{}", call.name, call.input));
            let _ = reply_tx.send(Reply { id: call.id, result: answer(&call) });
        }
        let (text, end) = engine.join().unwrap();
        (text, end, seen)
    }

    #[test]
    fn prints_and_returns() {
        let (text, end, _) = script("print('a', 1, {b: 2}); console.log('c'); return {d: [1]};", |_| Ok(String::new()));
        assert_eq!(end, End::Done);
        assert_eq!(text, "a 1 {\n  \"b\": 2\n}\nc\n{\n  \"d\": [\n    1\n  ]\n}\n");
    }

    #[test]
    fn calls_tools_concurrently_and_gets_their_text() {
        let code = r#"
            const [a, b] = await Promise.all([tools.echo({x: 1}), tools["mcp__a-b__c"]({y: "z"})]);
            return a + "|" + b;
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
            return [ALL_TOOLS.map(t => t.name).join(","), typeof require, typeof process, typeof fetch,
                    typeof globalThis.__send, typeof __settle];
        "#;
        let (text, end, _) = script(code, |_| Ok(String::new()));
        assert_eq!(end, End::Done);
        let shown: Json = serde_json::from_str(&text).unwrap();
        assert_eq!(shown, serde_json::json!(["echo,mcp__a-b__c", "undefined", "undefined", "undefined", "undefined", "function"]));
    }

    #[test]
    fn says_which_calls_were_not_awaited_and_numbers_lines_as_written() {
        let slow = |_: &Call| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            Ok(String::new())
        };
        let (text, end, _) = script("tools.echo({}); return 'done';", slow);
        assert_eq!(end, End::Done);
        assert_eq!(text, "done\n(1 tool call(s) the script did not await were cancelled.)\n");
        let (_, end, _) = script("const a = 1;\nnull.x;", |_| Ok(String::new()));
        assert!(matches!(&end, End::Threw(m) if m.contains(":2:")), "{end:?}");
    }

    #[test]
    fn runaway_recursion_and_allocation_are_script_errors() {
        // On a thread with the stack of tokio's blocking pool.
        let on_small_stack = |code: &'static str| {
            let thread = std::thread::Builder::new().stack_size(2 << 20);
            thread.spawn(move || script(code, |_| Ok(String::new())).1).unwrap().join().unwrap()
        };
        let end = on_small_stack("const f = (n) => f(n + 1) + 1; f(0);");
        assert!(matches!(&end, End::Threw(m) if m.contains("stack")), "{end:?}");
        let end = on_small_stack("const a = []; for (;;) a.push(new Array(1e6).fill(1));");
        assert!(matches!(&end, End::Threw(m) if m.contains("memory")), "{end:?}");
    }

    #[test]
    fn stop_interrupts_a_busy_script() {
        let (call_tx, _call_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_reply_tx, reply_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let engine = std::thread::spawn(move || run("print('before'); for (;;) {}", &[], call_tx, reply_rx, flag));
        std::thread::sleep(std::time::Duration::from_millis(100));
        stop.store(true, Ordering::Relaxed);
        let (text, end) = engine.join().unwrap();
        assert_eq!((text.as_str(), end), ("before\n", End::Stopped));
    }

    #[test]
    fn a_closed_reply_channel_stops_a_waiting_script() {
        let (call_tx, mut call_rx) = tokio::sync::mpsc::unbounded_channel();
        let (reply_tx, reply_rx) = std::sync::mpsc::channel::<Reply>();
        let stop = Arc::new(AtomicBool::new(false));
        let tools = [Callable { name: "echo".into(), description: String::new(), schema: Json::Null }];
        let engine = std::thread::spawn(move || run("await tools.echo({})", &tools, call_tx, reply_rx, stop));
        assert!(call_rx.blocking_recv().is_some());
        drop(reply_tx);
        assert_eq!(engine.join().unwrap().1, End::Stopped);
    }
}
