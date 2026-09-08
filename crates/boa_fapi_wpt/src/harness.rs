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
        "globalThis.__wpt = {{ results: [], file: {file_label:?} }};"
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
  function fmt(value) {
    try { return String(value); } catch (e) { return "<unformattable>"; }
  }
  function record_once(name, pass, message) {
    var key = String(name);
    var state = states[key];
    if (state === "passed" || state === "failed") {
      results.push({ kind: "test", name: key, pass: false, message: "duplicate terminal completion" });
      states[key] = "failed";
      return;
    }
    if (pass) {
      if (state === "failed") { return; }
      states[key] = "passed";
    } else {
      states[key] = "failed";
    }
    results.push({ kind: "test", name: key, pass: !!pass, message: safe_text(message || "") });
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
      record_once(name, false, safe_error(e));
      return undefined;
    }
  }
  globalThis.test = function(fn, name) {
    try {
      var t = { name: name, done: false, cleanup: [] };
      t.step = function(f) { return run_step(name, f, this, []); };
      t.step_func = function(f) { return function() { return run_step(name, f, this, arguments); }; };
      t.add_cleanup = function(f) { t.cleanup.push(f); };
      t.done = function() { t.done = true; };
      fn(t);
      try {
        for (var i = 0; i < t.cleanup.length; i++) { t.cleanup[i](); }
      } catch (e) {
        // Cleanup failure: FAIL without a second PASS.
        record_once(name, false, safe_error(e));
        return;
      }
      record_once(name, true, "");
    } catch (e) {
      record_once(name, false, safe_error(e));
    }
  };
  globalThis.async_test = function(name) {
    var t = { name: name, pending: true, cleanup: [] };
    t.step = function(f) { return run_step(name, f, this, []); };
    t.step_func = function(f) {
      return function() { return run_step(name, f, this, arguments); };
    };
    t.add_cleanup = function(f) { t.cleanup.push(f); };
    t.done = function() {
      // Second `done()` is a harness failure, not a silent pass: record
      // explicitly so duplicate completion breaks strict instead of
      // masquerading as PASS.
      if (!t.pending) {
        record_once(t.name, false, "async_test done() called twice");
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
    return t;
  };
  globalThis.promise_test = function(fn, name) {
    var label = name;
    var p;
    try {
      p = fn();
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
  globalThis.format_value = fmt;
})();
"#,
    );
    out
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
