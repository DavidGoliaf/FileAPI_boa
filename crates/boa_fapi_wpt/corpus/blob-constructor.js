// Adapted WPT: FileAPI/blob/Blob-constructor.any.js (subset).
// Upstream: https://github.com/web-platform-tests/wpt/blob/0968c868d8095217d18d86b34c7f21dccae58768/FileAPI/blob/Blob-constructor.any.js
// Adaptation: only the constructor-shape assertions supported by the M2
// Blob binding (no MessageChannel ports, no @@iterator protocol, no
// FrozenArray). Each adapted assertion keeps its upstream subtest name.
test(function() {
  assert_true("Blob" in globalThis);
  assert_equals(Blob.length, 0);
  assert_true(Blob instanceof Function);
}, "Blob interface object");
test(function() {
  var blob = new Blob();
  assert_true(blob instanceof Blob);
  assert_equals(String(blob), "[object Blob]");
  assert_equals(blob.size, 0);
  assert_equals(blob.type, "");
}, "Blob constructor with no arguments");
test(function() {
  assert_throws_js(TypeError, function() { var blob = Blob(); });
}, "Blob constructor with no arguments, without 'new'");
test(function() {
  var blob = new Blob(undefined);
  assert_true(blob instanceof Blob);
  assert_equals(blob.size, 0);
  assert_equals(blob.type, "");
}, "Blob constructor with undefined as first argument");
test(function() {
  var args = [null, true, false, 0, 1, 1.5, "FAIL", new Date(), {}, { 0: "FAIL", length: 1 }];
  args.forEach(function(arg) {
    assert_throws_js(TypeError, function() { new Blob(arg); });
  });
}, "Passing non-objects, Dates and RegExps for blobParts should throw a TypeError.");
test(function() {
  assert_throws_js(TypeError, function() { new Blob(true); });
}, "blobParts not an object: boolean");
test(function() {
  assert_throws_js(TypeError, function() { new Blob("fail"); });
}, "blobParts not an object: string");
test(function() {
  assert_throws_js(TypeError, function() { new Blob(7); });
}, "blobParts not an object: number");
test(function() {
  var blob = new Blob(["abc"]);
  assert_equals(blob.size, 3);
  assert_equals(blob.type, "");
}, "Blob constructor with string part");
test(function() {
  var blob = new Blob([new Uint8Array([0x50, 0x41, 0x53, 0x53])]);
  assert_equals(blob.size, 4);
}, "Passing typed arrays as elements of the blobParts array should work.");
test(function() {
  var blob = new Blob(["foo"]);
  var outer = new Blob([blob, blob]);
  assert_equals(outer.size, 6);
}, "Array with two blobs");
