// Adapted WPT: FileAPI/blob/Blob-slice.any.js (subset).
// Upstream: https://github.com/web-platform-tests/wpt/blob/0968c868d8095217d18d86b34c7f21dccae58768/FileAPI/blob/Blob-slice.any.js
// Adaptation: slice boundary/type assertions supported by the M1 slice
// semantics (no Blob URLs, no workers, no DOM).
test(function() {
  var blob = new Blob(["hello world"]);
  assert_equals(blob.slice(6, 11).size, 5);
  assert_equals(blob.slice(0, 5).size, 5);
}, "Blob slice positive range");
test(function() {
  var blob = new Blob(["hello world"]);
  assert_equals(blob.slice(-5).size, 5);
  assert_equals(blob.slice(0, -1).size, 10);
}, "Blob slice negative range");
test(function() {
  var blob = new Blob(["hello"]);
  assert_equals(blob.slice(3, 100).size, 2);
  assert_equals(blob.slice(5, 2).size, 0);
}, "Blob slice clamping and empty span");
test(function() {
  var blob = new Blob(["hello"], { type: "text/plain" });
  var sliced = blob.slice(0, 5, "TEXT/PLAIN");
  assert_equals(sliced.type, "text/plain");
  var empty = blob.slice(0, 5);
  assert_equals(empty.type, "");
}, "Blob slice content type normalization");
test(function() {
  var blob = new Blob(["hello world"]);
  var sliced = blob.slice(3, 8);
  assert_true(sliced instanceof Blob);
  assert_equals(sliced.size, 5);
  assert_equals(blob.size, 11);
}, "Blob slice returns a new Blob without mutating the original");
