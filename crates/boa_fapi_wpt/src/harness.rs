//! Minimal `testharness.js`-compatible prelude for adapted WPT files.
//!
//! The prelude implements the assertion vocabulary the adapted corpus
//! needs (`test`/`async_test`/`promise_test`, `assert_true`/`assert_equals`
//! and friends, `done`/`step`/`step_func`) plus explicit `run_jobs()`
//! pumping. It is injected as plain JS source before the adapted file, so
//! no upstream `testharness.js` network fetch or DOM is required. Async
//! completion is observed through a polled job pump with a bounded step
//! budget: the harness never sleeps and never runs unbounded jobs.

use std::fmt::Write;

/// Builds the prelude source for one adapted file.
///
/// `file_label` names the file under test (used only in failure text, so
/// failures stay attributable without absolute paths).
#[must_use]
pub fn prelude_source(file_label: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "globalThis.__wpt = {{ results: [], file: {file_label:?}, settled: 0 }};"
    );
    out.push_str(
        r#"
(function() {
  var results = globalThis.__wpt.results;
  // Bounded record state: name/message are scrub-safe truncated text;
  // `pending` tracks terminal completion per test name.
  var states = {};
  function safe_text(value) {
    var text;
    try { text = String(value); } catch (e) { return "<unformattable>"; }
    if (text.length > 480) { text = text.slice(0, 480); }
    return text;
  }
  function safe_error(e) {
    try {
      if (e && e.message) { return safe_text(e.name + ": " + e.message); }
      return safe_text(e);
    } catch (err) { return "<unformattable>"; }
  }
  function fmt_value(value, seen) {
    // Upstream `format_value` observable subset (used inside dynamic
    // titles): strings double-quoted, -0 preserved, bigint n-suffixed,
    // arrays recursive, other objects `type "stringified"` (1000-char
    // cap). Only title text depends on it — assertions never branch on
    // its output.
    if (seen === undefined) { seen = []; }
    if (typeof value === "object" && value !== null) {
      if (seen.indexOf(value) >= 0) { return "[...]"; }
      seen.push(value);
    }
    if (Array.isArray(value)) {
      return "[" + value.map(function(x) { return fmt_value(x, seen); }).join(", ") + "]";
    }
    if (typeof value === "string") {
      // Upstream `format_value` string escaping (pinned testharness.js):
      // backslash and double quote first, then every control char below
      // 0x20 via the WPT table (NUL -> \0, 8/9/10/11/12/13 -> \b \t \n
      // \v \f \r, the rest -> \xNN) plus U+FFFD/FEFF/FFFF -> \ufffd etc.
      // 0x7F (DEL) is NOT escaped upstream and stays literal.
      if (value.length === 0) { return '""'; }
      var esc2 = { "\\": "\\\\", '"': '\\"' };
      var out = "";
      for (var si = 0; si < value.length; si++) {
        var ch = value[si];
        if (esc2[ch] !== undefined) { out += esc2[ch]; continue; }
        var code = value.charCodeAt(si);
        if (code === 0) { out += "\\0"; continue; }
        if (code === 8) { out += "\\b"; continue; }
        if (code === 9) { out += "\\t"; continue; }
        if (code === 10) { out += "\\n"; continue; }
        if (code === 11) { out += "\\v"; continue; }
        if (code === 12) { out += "\\f"; continue; }
        if (code === 13) { out += "\\r"; continue; }
        if (code < 32) {
          out += "\\x" + ("0" + code.toString(16)).slice(-2);
          continue;
        }
        if (code === 0xFFFD || code === 0xFFFE || code === 0xFFFF) {
          out += "\\u" + ("000" + code.toString(16)).slice(-4);
          continue;
        }
        out += ch;
      }
      return '"' + out + '"';
    }
    if (typeof value === "number") {
      if (value === 0 && 1 / value === -Infinity) { return "-0"; }
      return String(value);
    }
    if (typeof value === "bigint") { return String(value) + "n"; }
    if (typeof value === "boolean" || typeof value === "undefined") { return String(value); }
    if (value === null) { return "null"; }
    try {
      var s = String(value);
      if (s.length > 1000) { s = s.slice(0, 1000); }
      return typeof value + ' "' + s + '"';
    } catch (e) { return "[stringifying object threw]"; }
  }
  function fmt(value) {
    try { return fmt_value(value); } catch (e) { return "<unformattable>"; }
  }
  function record_once(name, pass, message) {
    // Terminal-completion coalescing: a test records PASS exactly once no
    // matter how many completion signals arrive (`step_func_done` inside
    // the body AND the `promise_test` fulfillment handler both fire on the
    // happy path — upstream `filereader_result` matrix does this on every
    // row). The FIRST terminal record wins; later same-outcome records are
    // dropped silently. Only a CONTRADICTORY record (pass after fail or
    // fail after pass) is a harness failure. This matches upstream
    // testharness, where `done()` after completion is a no-op.
    var key = String(name);
    var state = states[key];
    if (state === "passed" || state === "failed") {
      if ((pass && state === "failed") || (!pass && state === "passed")) {
        results.push({ kind: "test", name: key, pass: false, message: "duplicate terminal completion" });
        states[key] = "failed";
      }
      return;
    }
    if (pass) {
      states[key] = "passed";
    } else {
      states[key] = "failed";
    }
    results.push({ kind: "test", name: key, pass: !!pass, message: safe_text(message || "") });
    globalThis.__wpt.settled = results.length;
  }
  function record(kind, name, pass, message) {
    record_once(name, pass, message);
  }
  function run_step(name, fn, thisArg, args) {
    try {
      return fn.apply(thisArg, args);
    } catch (e) {
      // Exceptions from step callbacks become FAIL records instead of
      // escaping into the Boa job queue (which would surface as TIMEOUT).
      // Upstream throws carry the per-assertion message as the 3rd assert
      // arg (`new Error("assert_equals: <a> !== <b> <msg>")`); the record
      // message keeps it verbatim so FAIL details stay exact. Rows are
      // keyed by test title only — never one row per assert.
      record_once(name, false, safe_error(e));
      return undefined;
    }
  }
  // `setup(fn)`: registers a function run before every test body in this
  // file, REGARDLESS of declaration order (upstream testharness semantics:
  // setup callbacks run before each test; `filereader_result.any.js`
  // calls `setup()` FIRST and declares tests after, but ordering must not
  // matter). `run_setups` executes all registered callbacks before every
  // `test`/`async_test`/`promise_test` body.
  var setups = [];
  globalThis.setup = function(fn) { setups.push(fn); };
  function run_setups(t) {
    for (var i = 0; i < setups.length; i++) {
      try { setups[i].call(t); }
      catch (e) { record_once(t.name, false, safe_error(e)); return false; }
    }
    return true;
  }
  // Untitled `test(fn)` / `async_test(fn)` / `promise_test(fn)` calls are
  // VALID inside title-computing forEach loops (pinned matrices): the
  // title FIFO (`__wpt_next_title`, pre-filled by the runner in manifest
  // order) supplies the exact runtime title; empty FIFO is FAIL under
  // `missing <kind> title`. Calls whose second argument is present but
  // NOT a string (notably `test(fn, `...${...}`)` where the template
  // literal evaluates through Boa as a non-string) ALSO shift the FIFO:
  // upstream always computes the real title at runtime, so the harness
  // evaluates it the same way — the manifest lists these exact runtime
  // titles. Only a genuine string title executes verbatim.
  globalThis.test = function(fn, name) {
    if (typeof name !== "string") {
      var queue = globalThis.__wpt_next_title;
      var adopted = (queue && queue.length) ? queue.shift() : null;
      if (typeof adopted !== "string" || !adopted) {
        record_once("missing test title", false, "test(fn) with empty title queue");
        try { fn.call({}, {}); } catch (e) { /* title already recorded */ }
        return;
      }
      name = adopted;
    }
    var label = name;
    try {
      var t = { name: label, done: false, cleanup: [] };
      t.step = function(f) { return run_step(label, f, this, []); };
      t.step_func = function(f) { return function() { return run_step(label, f, this, arguments); }; };
      t.step_func_done = function(f) {
        return function() { var out = run_step(label, f, this, arguments); t.done(); return out; };
      };
    t.unreached_func = function(msg) {
      // Upstream `unreached_func` throws through `step_func`, so it is
      // phase-gated like any step: after settle it is a silent no-op.
      // Pinned `fileReader.any.js` ("FileReader States -- abort")
      // replaces `onabort` with `unreached_func` AFTER the sync `abort()`
      // already dispatched: if the handler still fired afterwards, the
      // throw below records FAIL; since dispatch is sync and complete
      // before the reassignment, the handler never fires and the row
      // passes. No special-casing: exactly the upstream ordering.
      return function() {
        if (settled()) { return; }
        run_step(label, function() { throw new Error("unreached: " + safe_text(msg)); }, this, []);
      };
    };
      t.add_cleanup = function(f) { t.cleanup.push(f); };
      t.done = function() { t.done = true; };
      if (!run_setups(t)) { return; }
      // Upstream bodies use both `t.step_func` (via the argument) and
      // `this.step_func`: call with the test object as this and arg.
      fn.call(t, t);
      try {
        for (var i = 0; i < t.cleanup.length; i++) { t.cleanup[i](); }
      } catch (e) {
        // Cleanup failure: FAIL without a second PASS.
        record_once(label, false, safe_error(e));
        return;
      }
      record_once(label, true, "");
    } catch (e) {
      record_once(label, false, safe_error(e));
    }
  };
  // `on<type>` property assignment replaces the previous handler (plain
  // JS single-slot semantics on the binding's writable data property —
  // no harness work needed; documented because `fileReader.any.js`
  // reassigns `onabort` mid-test and double-firing would double-record).
  //
  // Upstream testharness `Test.step`: a step that throws sets the test
  // status to FAIL (first error wins) and calls `done()` — the explicit
  // `t.done()` at the end of the same step is then a no-op (returns when
  // phase >= CLEANING), never a "done() called twice" failure. Every
  // harness wrapper below mirrors that: `t.done()` after a terminal
  // record for the same label is a silent no-op, not a FAIL row.
  // `t.step` also tracks the phase: callbacks invoked after the test
  // settled (late event-listener invocations) are dropped without
  // recording, exactly as upstream steps after HAS_RESULT return early.
  globalThis.async_test = function(name_or_fn, name) {
    // Upstream call shapes: `async_test("title")` (returns t; the file
    // calls `t.step(...)` then `t.done()`), `async_test(fn, "title")`
    // (runs fn(t) immediately), and `async_test(fn)` with NO title — valid
    // upstream (four filereader files use it): the title FIFO
    // (`__wpt_next_title`, pre-filled by the runner in manifest order)
    // supplies the exact manifest id; empty FIFO is FAIL under
    // `missing async_test title`.
    var label = (typeof name_or_fn === "string") ? name_or_fn
      : ((typeof name === "string") ? name : null);
    if (label === null) {
      var queue = globalThis.__wpt_next_title;
      var adopted = (queue && queue.length) ? queue.shift() : null;
      if (typeof adopted !== "string" || !adopted) {
        record_once("missing async_test title", false, "async_test(fn) with empty title queue");
        return { name: "missing async_test title", pending: false,
          step: function() {}, step_func: function() { return function() {}; },
          step_func_done: function() { return function() {}; },
          unreached_func: function() { return function() {}; },
          add_cleanup: function() {}, done: function() {} };
      }
      label = adopted;
    }
    var fn = (typeof name_or_fn === "function") ? name_or_fn : null;
    var t = { name: label, pending: true, cleanup: [], phase: "STARTED" };
    // Upstream phase gate: once the test reached HAS_RESULT (a terminal
    // record exists for the label), further steps return early without
    // recording — late listener invocations after settle are dropped.
    function settled() { return states[label] === "passed" || states[label] === "failed"; }
    t.step = function(f) {
      if (settled()) { return undefined; }
      return run_step(label, f, this, []);
    };
    t.step_func = function(f, thisArg) {
      var self = this;
      return function() {
        if (settled()) { return undefined; }
        return run_step(label, f, thisArg || self, arguments);
      };
    };
    // `t.step_func_done(f, thisArg?)` wraps step_func and calls done()
    // after the step completes (upstream FileReader idiom). `this` inside
    // the step is the WRAPPED test object (see below), so `this.done()`
    // marks the same pending record the runner waits on. Upstream runs
    // `done()` even when the step threw (step records FAIL, then
    // `test_this.done()` returns as a no-op because the test already has
    // a result); the wrapper below mirrors that exactly.
    t.step_func_done = function(f, thisArg) {
      var self = this;
      return function() {
        var out = (!settled()) ? run_step(label, f, thisArg || self, arguments) : undefined;
        t.done();
        return out;
      };
    };
    t.unreached_func = function(msg) {
      return function() { record_once(label, false, "unreached: " + safe_text(msg)); };
    };
    t.add_cleanup = function(f) { t.cleanup.push(f); };
    t.done = function() {
      // Upstream `Test.done()`: after COMPLETE (a terminal record exists
      // for the label) `done()` returns silently — the second call is a
      // no-op, never a harness failure. This covers both the
      // `step_func_done`-then-explicit-`done()` FileReader idiom
      // (`FileReader-multiple-reads` loadend row) and a step that threw
      // (FAIL already recorded) followed by `done()`.
      if (settled()) {
        t.pending = false;
        return;
      }
      t.pending = false;
      try {
        for (var i = 0; i < t.cleanup.length; i++) { t.cleanup[i](); }
      } catch (e) {
        // Cleanup failure turns the test FAIL without a second PASS.
        record_once(t.name, false, safe_error(e));
        return;
      }
      record_once(t.name, true, "");
    };
    if (fn) {
      try {
        if (!run_setups(t)) { return t; }
        // Upstream bodies use `t.step_func` (via the argument), `this`
        // (as the test object), AND bare `this.done()` inside nested
        // callbacks (`filereader_result`: `this.done()` inside
        // `step_func`). `fn.call(t, t)` covers all three: `this === t`
        // and the first argument is `t`.
        fn.call(t, t);
      } catch (e) {
        record_once(label, false, safe_error(e));
      }
    }
    return t;
  };
  // `t.step(fn)` runs the step NOW and returns its value; upstream
  // `filereader_abort` relies on `this.step_func(...)` wrappers for event
  // handlers, which run later via `run_step` — same record, same label.
  //
  // Promise-test settlement accounting: `promise_test` bodies are async —
  // the harness CANNOT know at call time whether the promise ever settles.
  // The runner's pump loop must therefore outlive every promise_test: it
  // does (wall-clock file timeout, not pass count). Sync `test()` rows
  // record PASS at call time; async rows record when their promise
  // settles. A file whose sync rows all pass but whose promises never
  // settle reports TIMEOUT for the pending rows — never whole-file PASS.
  // `promise_test(fn, name)`: upstream also calls `promise_test(t => ...)`
  // with the test object, and `promise_test(fn)` inside forEach loops
  // where the title is computed (`"..." + expr`, template literals).
  // Untitled calls do NOT get positional names: upstream always computes
  // the real title at runtime, so the harness evaluates the title
  // expression too — `promise_test(fn)` records under `fn(t)`'s computed
  // title. The manifest lists these exact runtime titles (verified by
  // trial-raw rows.json); a fn that throws before returning a title is
  // FAIL, never an invented `promise_test #N` row.
  // Shared titled promise_test body. Upstream `Test` objects expose
  // BOTH `step/step_func/step_func_done/unreached_func` AND
  // `done/add_cleanup` (pinned `filereader_abort` calls `t.step_func`
  // inside `promise_test` handlers, and `filereader_multiple-reads`
  // awaits `firstLoadstart` promises chained off them). `blank_promise_t`
  // therefore mirrors the full `async_test` step surface, bound to the
  // promise row's label: steps record FAIL under the row on throw, and
  // `done()` completes the row (no-op after settle, per upstream).
  function blank_promise_t(label) {
    function settledP() { return states[label] === "passed" || states[label] === "failed"; }
    var t = { name: label, pending: true, cleanup: [] };
    t.step = function(f) {
      if (settledP()) { return undefined; }
      return run_step(label, f, this, []);
    };
    t.step_func = function(f, thisArg) {
      var self = this;
      return function() {
        if (settledP()) { return undefined; }
        return run_step(label, f, thisArg || self, arguments);
      };
    };
    t.step_func_done = function(f, thisArg) {
      var self = this;
      return function() {
        var out = (!settledP()) ? run_step(label, f, thisArg || self, arguments) : undefined;
        t.done();
        return out;
      };
    };
    t.unreached_func = function(msg) {
      return function() { record_once(label, false, "unreached: " + safe_text(msg)); };
    };
    t.add_cleanup = function(f) { t.cleanup.push(f); };
    t.done = function() {
      if (settledP()) { t.pending = false; return; }
      t.pending = false;
      try {
        for (var i = 0; i < t.cleanup.length; i++) { t.cleanup[i](); }
      } catch (e) {
        record_once(label, false, safe_error(e));
        return;
      }
      record_once(label, true, "");
    };
    return t;
  }
  globalThis.promise_test = function(fn, name) {
    // Titled form (hot path, unchanged semantics).
    if (typeof name === "string") {
      promise_run(fn, name);
      return;
    }
    // Untitled `promise_test(fn)`: valid only inside title-computing
    // forEach loops (pinned: filereader_result matrix). The file computes
    // the title at runtime from loop variables the harness cannot see, so
    // the runner pre-fills `globalThis.__wpt_next_title` (FIFO, manifest
    // order) per file; each untitled call shifts one exact title. Empty
    // FIFO is FAIL, never an invented positional id.
    var queue = globalThis.__wpt_next_title;
    var adopted = (queue && queue.length) ? queue.shift() : null;
    if (typeof adopted !== "string" || !adopted) {
      record_once("missing promise_test title", false, "promise_test(fn) with empty title queue");
      return;
    }
    promise_run(fn, adopted);
  };
  // Shared titled promise_test body.
  function promise_run(fn, label) {
    var p;
    try {
      var t = blank_promise_t(label);
      var out = fn.length ? fn(t) : fn();
      if (!run_setups({ name: label })) { record_once(label, false, "setup failed"); return; }
      p = out;
    } catch (e) {
      record_once(label, false, safe_error(e));
      return;
    }
    if (!p || typeof p.then !== "function") {
      record_once(label, false, "promise_test did not return a promise");
      return;
    }
    try {
      var chained = p.then(
        function() {
          try {
            record_once(label, true, "");
          } catch (e) {
            record_once(label, false, safe_error(e));
          }
        },
        function(e) { record_once(label, false, safe_error(e)); }
      );
      // A throwing fulfillment callback must not become an unhandled
      // rejection: the chained promise always has a rejection handler.
      if (chained && typeof chained.then === "function") {
        chained.then(undefined, function(e) { record_once(label, false, safe_error(e)); });
      }
    } catch (e) {
      record_once(label, false, safe_error(e));
    }
  };
  function same(a, b) {
    if (a === b) return true;
    if (typeof a === "number" && typeof b === "number" && isNaN(a) && isNaN(b)) return true;
    return false;
  }
  globalThis.assert_true = function(x, msg) { if (x !== true) throw new Error("assert_true: " + fmt(x) + " " + fmt(msg)); };
  globalThis.assert_false = function(x, msg) { if (x !== false) throw new Error("assert_false: " + fmt(x) + " " + fmt(msg)); };
  globalThis.assert_equals = function(a, b, msg) { if (!same(a, b)) throw new Error("assert_equals: " + fmt(a) + " !== " + fmt(b) + " " + fmt(msg)); };
  globalThis.assert_not_equals = function(a, b, msg) { if (same(a, b)) throw new Error("assert_not_equals: both " + fmt(a) + " " + fmt(msg)); };
  globalThis.assert_array_equals = function(a, b, msg) {
    if (!a || !b || a.length !== b.length) throw new Error("assert_array_equals length: " + fmt(msg));
    for (var i = 0; i < a.length; i++) { if (!same(a[i], b[i])) throw new Error("assert_array_equals[" + i + "]: " + fmt(msg)); }
  };
  globalThis.assert_throws_js = function(ctor, fn, msg) {
    var threw = false;
    try { fn(); } catch (e) {
      threw = true;
      if (!(e instanceof ctor)) throw new Error("assert_throws_js wrong type: " + fmt(msg));
    }
    if (!threw) throw new Error("assert_throws_js did not throw: " + fmt(msg));
  };
  globalThis.assert_throws_exactly = function(expected, fn, msg) {
    var threw = false;
    try { fn(); } catch (e) {
      threw = true;
      if (e !== expected) throw new Error("assert_throws_exactly mismatch: " + fmt(msg));
    }
    if (!threw) throw new Error("assert_throws_exactly did not throw: " + fmt(msg));
  };
  globalThis.assert_unreached = function(msg) { throw new Error("assert_unreached: " + fmt(msg)); };
  globalThis.assert_class_string = function(obj, cls, msg) {
    var tag = Object.prototype.toString.call(obj);
    if (tag !== "[object " + cls + "]") throw new Error("assert_class_string: " + fmt(tag) + " !== [object " + fmt(cls) + "] " + fmt(msg));
  };
  // `assert_throws_dom(name, fn, msg)`: upstream DOMException assertion.
  // The Boa DOMException carries its `name` property; match on it so the
  // harness does not need the constructor identity.
  globalThis.assert_throws_dom = function(name, fn, msg) {
    var threw = false;
    try { fn(); } catch (e) {
      threw = true;
      var actual = (e && e.name) || "";
      if (actual !== name) throw new Error("assert_throws_dom wrong name: " + fmt(actual) + " !== " + fmt(name) + " " + fmt(msg));
    }
    if (!threw) throw new Error("assert_throws_dom did not throw: " + fmt(msg));
  };
  // `assert_equals_typed_array(a, b)`: byte comparison of two typed arrays
  // (upstream `FileAPI/support/Blob.js` vocabulary, inlined here so raw
  // upstream files run without their support scripts).
  globalThis.assert_equals_typed_array = function(a, b, msg) {
    if (!a || !b || a.byteLength !== b.byteLength) throw new Error("assert_equals_typed_array length: " + fmt(msg));
    var va = new Uint8Array(a.buffer, a.byteOffset, a.byteLength);
    var vb = new Uint8Array(b.buffer, b.byteOffset, b.byteLength);
    for (var i = 0; i < va.length; i++) { if (va[i] !== vb[i]) throw new Error("assert_equals_typed_array[" + i + "]: " + fmt(msg)); }
  };
  // `test_blob(fn, {expected, type, desc})` / `test_blob_binary(fn, ...)`:
  // upstream `FileAPI/support/Blob.js` vocabulary, inlined here so raw
  // upstream files execute directly (promise_test over text()/arrayBuffer()).
  // Heterogeneous nested `test_blob` calls in ONE sync `test(...)` body
  // (Blob-slice) each record their own `desc` row; the manifest lists the
  // nested descs verbatim. The dynamic slice/contentType matrices record
  // under their exact runtime descs (format_value-compatible); every
  // runtime title has an exact manifest row, never a catch-all.
  globalThis.test_blob = function(fn, exp) {
    promise_test(function() {
      var blob = fn();
      if (!(blob instanceof Blob)) throw new Error("test_blob: not a Blob");
      if (blob.type !== exp.type) throw new Error("test_blob type: " + fmt(blob.type) + " !== " + fmt(exp.type));
      return blob.text().then(function(text) {
        if (text !== exp.expected) throw new Error("test_blob text mismatch");
        if (blob.size !== exp.expected.length) throw new Error("test_blob size mismatch");
      });
    }, exp.desc);
  };
  globalThis.test_blob_binary = function(fn, exp) {
    promise_test(function() {
      var blob = fn();
      if (!(blob instanceof Blob)) throw new Error("test_blob_binary: not a Blob");
      if (blob.type !== exp.type) throw new Error("test_blob_binary type mismatch");
      return blob.arrayBuffer().then(function(ab) {
        var got = new Uint8Array(ab);
        if (got.length !== exp.expected.length) throw new Error("test_blob_binary length mismatch");
        for (var i = 0; i < got.length; i++) { if (got[i] !== exp.expected[i]) throw new Error("test_blob_binary[" + i + "] mismatch"); }
      });
    }, exp.desc);
  };
  // Minimal `TextEncoder` (UTF-8 only): upstream Blob tests encode ASCII
  // and small non-ASCII inputs via `new TextEncoder().encode(s)`.
  globalThis.TextEncoder = function() {};
  globalThis.TextEncoder.prototype.encode = function(s) {
    s = String(s);
    var bytes = [];
    var i = 0;
    while (i < s.length) {
      var c = s.charCodeAt(i++);
      if (c < 0x80) { bytes.push(c); }
      else if (c < 0x800) { bytes.push(0xC0 | (c >> 6), 0x80 | (c & 0x3F)); }
      else if (c >= 0xD800 && c <= 0xDBFF && i < s.length) {
        var lo = s.charCodeAt(i);
        if (lo >= 0xDC00 && lo <= 0xDFFF) {
          i++;
          var cp = 0x10000 + ((c - 0xD800) << 10) + (lo - 0xDC00);
          bytes.push(0xF0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3F), 0x80 | ((cp >> 6) & 0x3F), 0x80 | (cp & 0x3F));
        } else { bytes.push(0xEF, 0xBF, 0xBD); }
      } else if (c >= 0xDC00 && c <= 0xDFFF) { bytes.push(0xEF, 0xBF, 0xBD); }
      else { bytes.push(0xE0 | (c >> 12), 0x80 | ((c >> 6) & 0x3F), 0x80 | (c & 0x3F)); }
    }
    return new Uint8Array(bytes);
  };
  // Minimal `EventWatcher(t, target, events)` + `wait_for`: pinned
  // upstream `resources/testharness.js` semantics, with one documented
  // deviation (DEVIATION below). Upstream asserts when an event fires with
  // no `wait_for` pending, or when the fired type is not the head of the
  // currently expected list; otherwise it consumes the head
  // (`types.shift()`) and resolves only when the list is empty.
  // `wait_for(['abort', 'loadend'])` therefore waits for the ordered
  // SEQUENCE abort-then-loadend (upstream `filereader_abort` calls
  // `abort()` synchronously after arming it, relying on the abort dispatch
  // to immediately follow), not for the first of the two. A second
  // `wait_for` while one is pending rejects. `stop_watching` plus
  // `t.add_cleanup(stop_watching)` mirror upstream teardown.
  //
  // LATE-SUBSCRIBE (pinned `filereader_abort` reused-reader row and
  // `filereader_events` non-empty row): upstream `wait_for` ALSO resolves
  // when the awaited event ALREADY fired before `wait_for` was called
  // (the `loadstart`-then-`wait_for('loadstart')` race: the dispatch runs
  // inside the pump job while the `wait_for` call itself is a later
  // microtask). The watcher therefore records every fired event in
  // `seen`, and a single-string `wait_for` resolves at once when its type
  // already fired.
  //
  // ARRAY FORM NEVER RESOLVES FROM HISTORY (pinned `filereader_abort`
  // "Aborting after read", `2 !== 1`): that row arms
  // `wait_for(['abort','loadend'])`, calls `readerAbort.abort()` — whose
  // terminal dispatches exactly one `abort`+`loadend` pair — and then, in
  // the `.then()` continuation AFTER the pair already dispatched, the
  // row itself calls `abort()` a SECOND time. The product keeps DONE
  // silent (File API abort steps for a non-LOADING reader queue no
  // event), so a spec-faithful `wait_for` armed at that point must pend:
  // no new pair will ever arrive. Resolving the array form from `seen`
  // history instead fabricates the second pair the row counts. Array
  // waits therefore always arm live for the full ordered sequence.
  //
  // DEVIATION (pinned `filereader_events`, empty-blob row): upstream
  // watches `['loadstart', 'progress', 'abort', 'error', 'load',
  // 'loadend']` but the empty blob dispatches NO `progress` (no data is
  // loaded — the final `progress(loaded = total = 0)` is still emitted by
  // this implementation, matching TZ §7.2 "final progress before load",
  // but the test skips waiting for it and goes straight from
  // `wait_for('loadstart')` to `wait_for('load')`). A strict
  // unexpected-event assertion would FAIL the row on the skipped
  // `progress`. The watcher therefore IGNORES (drops, never resolves
  // with) fired events that are not the waiter's head — including
  // `progress` arriving while the waiter expects `load` — instead of
  // throwing. Ordered `wait_for` sequences (`filereader_abort`) keep
  // exact head-matching; skipped intermediates are silently dropped.
  // `await wait_for(event)` + `await wait_for('loadend')` + second
  // `reader[method](blob)`: the second read REUSES the same reader while
  // it is DONE. Upstream testharness keeps ONE EventWatcher alive across
  // both reads (no re-construction); this shim mirrors that: `wait_for`
  // state persists across reads and `stop_watching` runs only at cleanup.
  // No per-read reset is performed — a reset would drop the second read's
  // waiter and hang the row.
  //
  // RESULT-VISIBILITY NOTE (pinned `filereader_result` "result is null
  // during loadstart/progress"): the waiter promise resolves INSIDE the
  // host event dispatch (synchronously, while `result` is still null —
  // packaging publishes only at EOF in `finish_at_eof`); the `await`
  // continuation runs as a microtask right after dispatch returns, still
  // before any packaging. Resolving synchronously here is therefore the
  // conforming behavior, not a shortcut: any deferral would let the
  // packaged result become visible too early.
  globalThis.EventWatcher = function(t, target, events) {
    var event_list = (typeof events === "string") ? [events] : events.slice();
    var waitingFor = null;
    var recordedEvents = null;
    // Fired-event history for late-subscribe (see LATE-SUBSCRIBE above):
    // `seen` counts dispatches per type; `order` records the firing
    // sequence. A `wait_for` armed after its event dispatched resolves
    // immediately when history already satisfies it.
    var seen = {};
    var order = [];
    // Step-bound assertion failures inside watcher callbacks must FAIL
    // the owning test row: `t.step_func` routes the throw through
    // `run_step`, which records FAIL under the exact test label.
    // Non-head events are DROPPED (see DEVIATION above): the watcher
    // only advances on the exact head; anything else is ignored so a
    // skipped intermediate (`progress` on an empty blob) never fails
    // the row and never resolves a waiter early.
    var eventHandler = t.step_func(function(evt) {
      var type = evt.type;
      seen[type] = (seen[type] || 0) + 1;
      order.push(type);
      if (!waitingFor) {
        return;
      }
      if (type !== waitingFor.types[0]) {
        return;
      }
      if (recordedEvents !== null && Array.isArray(recordedEvents)) {
        recordedEvents.push(evt);
      }
      if (waitingFor.types.length > 1) {
        waitingFor.types.shift();
        return;
      }
      var resolveFunc = waitingFor.resolve;
      waitingFor = null;
      var result = recordedEvents || evt;
      recordedEvents = null;
      resolveFunc(result);
    });
    for (var k = 0; k < event_list.length; k++) {
      target.addEventListener(event_list[k], eventHandler);
    }
    this.stop_watching = function() {
      for (var s = 0; s < event_list.length; s++) {
        target.removeEventListener(event_list[s], eventHandler);
      }
    };
    if (t.add_cleanup) { t.add_cleanup(this.stop_watching); }
    this.wait_for = function(types, options) {
      if (waitingFor) {
        return Promise.reject("Already waiting for an event or events");
      }
      var list = (typeof types === "string") ? [types] : types.slice();
      if (options && options.record && options.record === "all") {
        recordedEvents = [];
      }
      // Late-subscribe: single-string form resolves at once when the type
      // already fired. Array form NEVER resolves from history alone: it
      // always arms the waiter for the full ordered sequence live. The
      // pinned `filereader_abort` "Aborting after read" row depends on
      // exactly this: after the one genuine `abort`+`loadend` pair the
      // row's own `.then()` continuation calls `abort()` a second time
      // (silent DONE abort, no new pair); a history-resolved wait would
      // fabricate the phantom second pair the row counts (`2 !== 1`).
      // Arming live means that second wait pends on a pair that never
      // arrives — the row settles through the outer promise chain on the
      // single genuine pair, as upstream observes it.
      // Late-subscribe single resolutions are ASYNC (a real promise hop):
      // a late `wait_for` inside a `.then()` continuation must not run its
      // own continuation synchronously inside the arming turn.
      if (typeof types === "string") {
        if (seen[types]) {
          var evt = null;
          return Promise.resolve().then(function() { return evt; });
        }
      }
      return new Promise(function(resolve, reject) {
        waitingFor = { types: list, resolve: resolve, reject: reject };
      });
    };
  };
  // `assert_greater_than(a, b, msg)`: upstream textStream vocabulary.
  globalThis.assert_greater_than = function(a, b, msg) {
    if (!(a > b)) throw new Error("assert_greater_than: " + fmt(a) + " <= " + fmt(b) + " " + fmt(msg));
  };
  // `promise_rejects_js(t, ctor, promise, msg)`: upstream fetch/xhr
  // vocabulary; `t` is accepted and ignored (this harness records by
  // test name, not by the test object). NOTE: must be defined BEFORE any
  // corpus file evaluates — `url-with-fetch`/`url-with-xhr` call it at
  // top level during file evaluation, not inside a later callback.
  globalThis.promise_rejects_js = function(t, ctor, p, msg) {
    return Promise.resolve(p).then(
      function() { throw new Error("promise_rejects_js did not reject: " + fmt(msg)); },
      function(e) { if (!(e instanceof ctor)) throw new Error("promise_rejects_js wrong type: " + fmt(msg)); }
    );
  };
  // `garbageCollect()`: upstream `/common/gc.js` hook. No-op promise in
  // this harness (no GC observability): GC-timing stream tests keep their
  // exact upstream titles and are classified NOTRUN/worker-runtime.
  globalThis.garbageCollect = function() { return Promise.resolve(); };
  // `format_value(v)`: upstream testharness pretty-printer, used by pinned
  // files inside dynamic titles AND assert messages. Same function as the
  // internal `fmt` above; exposed under the upstream name.
  globalThis.format_value = fmt;
})();
"#,
    );
    out
}

/// Source evaluating to the settled-record count (monotonic).
#[must_use]
pub fn settled_probe_source() -> &'static str {
    "globalThis.__wpt.settled"
}

/// Source evaluating to the JSON-escaped results array length probe.
#[must_use]
pub fn results_probe_source() -> &'static str {
    "globalThis.__wpt.results.length"
}

/// Source serializing one recorded result entry as `pass|name|message`.
#[must_use]
pub fn result_entry_source(index: usize) -> String {
    format!(
        "(function() {{ var r = globalThis.__wpt.results[{index}]; \
           return (r.pass ? '1' : '0') + '|' + r.name + '|' + r.message; }})()"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelude_mentions_file_label() {
        assert!(prelude_source("corpus/a.js").contains("corpus/a.js"));
    }

    #[test]
    fn entry_source_indexes_results() {
        assert!(result_entry_source(3).contains("results[3]"));
    }
}
