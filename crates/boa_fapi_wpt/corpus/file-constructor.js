// Adapted WPT: FileAPI/file/File-constructor.any.js (subset).
// Upstream: https://github.com/web-platform-tests/wpt/blob/0968c868d8095217d18d86b34c7f21dccae58768/FileAPI/file/File-constructor.any.js
// Adaptation: constructor/name/type/lastModified assertions supported by
// the M2 File binding (no HTMLInputElement files, no lastModifiedDate).
test(function() {
  var file = new File(["hello"], "hello.txt");
  assert_true(file instanceof File);
  assert_true(file instanceof Blob);
  assert_equals(file.name, "hello.txt");
  assert_equals(file.size, 5);
}, "File constructor basic");
test(function() {
  var file = new File(["a/b"], "a/b.txt");
  assert_equals(file.name, "a:b.txt");
}, "File name slashes become colons");
test(function() {
  var file = new File(["x"], "x.txt", { type: "TEXT/PLAIN" });
  assert_equals(file.type, "text/plain");
}, "File type normalization");
test(function() {
  var file = new File(["x"], "x.txt", { lastModified: 12345 });
  assert_equals(file.lastModified, 12345);
}, "File lastModified supplied");
test(function() {
  assert_throws_js(TypeError, function() { new File(["x"]); });
  assert_throws_js(TypeError, function() { new File(); });
}, "File constructor requires bits and name");
