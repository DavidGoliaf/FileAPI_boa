// Adapted WPT: FileAPI/reading-data-section/blob-text.any.js (subset).
// Upstream: https://github.com/web-platform-tests/wpt/blob/0968c868d8095217d18d86b34c7f21dccae58768/FileAPI/reading-data-section/blob-text.any.js
// Adaptation: promise reads settled through explicit run_jobs() pumping;
// assertions use promise_test so the runner observes async settlement.
promise_test(function() {
  return new Blob(["abc"]).text().then(function(text) {
    assert_equals(text, "abc");
  });
}, "Blob text() decodes ASCII");
promise_test(function() {
  return new Blob([]).text().then(function(text) {
    assert_equals(text, "");
  });
}, "Blob text() of empty blob");
promise_test(function() {
  return new Blob(["a", "b", "c"]).text().then(function(text) {
    assert_equals(text, "abc");
  });
}, "Blob text() concatenates parts");
promise_test(function() {
  var file = new File(["xyz"], "f.txt");
  return file.text().then(function(text) {
    assert_equals(text, "xyz");
  });
}, "File inherits text()");
promise_test(function() {
  return new Blob(["abc"]).arrayBuffer().then(function(buffer) {
    assert_equals(buffer.byteLength, 3);
    assert_equals(new Uint8Array(buffer)[0], 97);
  });
}, "Blob arrayBuffer() returns exact bytes");
