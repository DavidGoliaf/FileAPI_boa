// Adapted WPT: FileAPI/FileReader/FileReader-readAsText.any.js (subset).
// Upstream: https://github.com/web-platform-tests/wpt/blob/0968c868d8095217d18d86b34c7f21dccae58768/FileAPI/FileReader/FileReader-readAsText.any.js
// Adaptation: async FileReader event flow with explicit run_jobs()
// pumping; assertions observe events/results, never Rust internals.
test(function() {
  var reader = new FileReader();
  assert_equals(reader.readyState, FileReader.EMPTY);
  assert_equals(reader.result, null);
  assert_equals(reader.error, null);
}, "FileReader initial state");
test(function() {
  var reader = new FileReader();
  reader.readAsText(new Blob(["hello"]));
  assert_equals(reader.readyState, FileReader.LOADING);
}, "FileReader readAsText enters LOADING");
test(function() {
  var reader = new FileReader();
  var blob = new Blob(["sync-text"]);
  var log = [];
  reader.onload = function() { log.push(reader.result); };
  reader.readAsText(blob);
  assert_equals(log.length, 0);
}, "FileReader result is pending before jobs run");
test(function() {
  assert_equals(FileReader.EMPTY, 0);
  assert_equals(FileReader.LOADING, 1);
  assert_equals(FileReader.DONE, 2);
}, "FileReader constants");
