// M9-E FileList host fixture surface (project-owned, NOT upstream WPT).
// The runner creates two distinct Files via the public host API
// (`FileApiHandle::file_from_bytes`), builds a real FileList via
// `FileApiHandle::file_list`, and injects ONLY that object as
// `globalThis.__wpt_file_list` before this file runs. Every assertion
// below observes the injected host object; a plain Array or a pair of
// Files never satisfies them.
test(function() {
  assert_equals(typeof FileList, "undefined",
    "no public FileList constructor");
}, "FileList has no public constructor");

test(function() {
  var list = globalThis.__wpt_file_list;
  assert_equals(Object.prototype.toString.call(list), "[object FileList]");
  assert_equals(list.length, 2);
}, "host FileList length and brand");

test(function() {
  var list = globalThis.__wpt_file_list;
  var first = list.item(0);
  var second = list.item(1);
  assert_true(first instanceof File);
  assert_true(second instanceof File);
  assert_equals(first.name, "a.txt");
  assert_equals(second.name, "b.txt");
  assert_equals(list[0], first, "indexed access is identity");
  assert_equals(list[1], second, "indexed access is identity");
  assert_true(list.item(0) === first, "item() is identity");
}, "host FileList identity and order");

test(function() {
  var list = globalThis.__wpt_file_list;
  assert_equals(list.item(2), null, "out-of-range item() is null");
  assert_equals(list[2], undefined, "out-of-range index is undefined");
  assert_equals(list.item(-1), null, "negative item() is null");
}, "host FileList out-of-range behavior");

test(function() {
  var list = globalThis.__wpt_file_list;
  // length is a prototype getter (Web IDL readonly attribute), not an
  // own data property: the getter reads the host brand, indexed slots
  // stay own data properties (see next assertions).
  assert_equals(list.length, 2);
  assert_equals(
    Object.prototype.hasOwnProperty.call(list, "length"), false);
}, "host FileList descriptors");

test(function() {
  // Indexed-getter iterator only (matches the binding contract):
  // Symbol.iterator aliases Array.prototype.values; no entries/keys/
  // values/forEach helpers.
  var list = globalThis.__wpt_file_list;
  assert_true(Symbol.iterator in list);
  assert_equals(list[Symbol.iterator], Array.prototype.values);
  assert_equals(list.values, undefined);
  assert_equals(list.keys, undefined);
  assert_equals(list.entries, undefined);
  assert_equals(list.forEach, undefined);
  var names = [];
  for (var file of list) names.push(file.name);
  assert_equals(names.join(","), "a.txt,b.txt");
}, "host FileList iteration contract");
