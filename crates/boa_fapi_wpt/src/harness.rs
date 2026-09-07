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
  function record(kind, name, pass, message) {
    results.push({ kind: kind, name: String(name), pass: !!pass, message: String(message || "") });
  }
  function fmt(value) {
    try { return String(value); } catch (e) { return "<unformattable>"; }
  }
  globalThis.test = function(fn, name) {
    try {
      var t = { name: name, done: false, cleanup: [] };
      t.step = function(f) { return f(); };
      t.step_func = function(f) { return f; };
      t.add_cleanup = function(f) { t.cleanup.push(f); };
      t.done = function() { t.done = true; };
      fn(t);
      for (var i = 0; i < t.cleanup.length; i++) { t.cleanup[i](); }
      record("test", name, true, "");
    } catch (e) {
      record("test", name, false, (e && e.message) ? (e.name + ": " + e.message) : fmt(e));
    }
  };
  globalThis.async_test = function(name) {
    var t = { name: name, pending: true, cleanup: [] };
    t.step = function(f) { return f(); };
    t.step_func = function(f) { return function() { return f.apply(this, arguments); }; };
    t.add_cleanup = function(f) { t.cleanup.push(f); };
    t.done = function() {
      t.pending = false;
      record("test", t.name, true, "");
      for (var i = 0; i < t.cleanup.length; i++) { t.cleanup[i](); }
    };
    return t;
  };
  globalThis.promise_test = function(fn, name) {
    var label = name;
    try {
      var p = fn();
      if (p && typeof p.then === "function") {
        p.then(
          function() { record("test", label, true, ""); },
          function(e) { record("test", label, false, (e && e.message) ? (e.name + ": " + e.message) : fmt(e)); }
        );
      } else {
        record("test", label, false, "promise_test did not return a promise");
      }
    } catch (e) {
      record("test", label, false, (e && e.message) ? (e.name + ": " + e.message) : fmt(e));
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
