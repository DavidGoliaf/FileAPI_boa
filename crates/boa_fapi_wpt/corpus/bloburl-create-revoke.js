// Adapted WPT: FileAPI/BlobURL/create-revoke.any.js (subset).
// Upstream: https://github.com/web-platform-tests/wpt/blob/0968c868d8095217d18d86b34c7f21dccae58768/FileAPI/BlobURL/create-revoke.any.js
// Adaptation: create/revoke/isolation assertions supported by the M6 URL
// shim (no Fetch dereference, no navigation, no workers orchestration).
test(function() {
  var url = URL.createObjectURL(new Blob(["x"]));
  assert_true(typeof url === "string");
  assert_true(url.indexOf("blob:") === 0);
  URL.revokeObjectURL(url);
}, "URL.createObjectURL returns a blob: URL");
test(function() {
  var a = URL.createObjectURL(new Blob(["a"]));
  var b = URL.createObjectURL(new Blob(["b"]));
  assert_true(a !== b);
  URL.revokeObjectURL(a);
  URL.revokeObjectURL(b);
}, "Two URLs never collide");
test(function() {
  assert_throws_js(TypeError, function() { URL.createObjectURL({}); });
  assert_throws_js(TypeError, function() { URL.createObjectURL("blob:x"); });
}, "URL.createObjectURL rejects non-Blob brands");
test(function() {
  var url = URL.createObjectURL(new Blob(["r"]));
  URL.revokeObjectURL(url);
  URL.revokeObjectURL(url);
  assert_equals(URL.revokeObjectURL("blob:missing"), undefined);
}, "URL.revokeObjectURL is idempotent and silent");
test(function() {
  assert_throws_js(TypeError, function() { URL.revokeObjectURL(); });
}, "URL.revokeObjectURL requires an argument");
test(function() {
  var fileUrl = URL.createObjectURL(new File(["f"], "f.txt"));
  assert_true(typeof fileUrl === "string");
  URL.revokeObjectURL(fileUrl);
}, "URL.createObjectURL accepts File");
