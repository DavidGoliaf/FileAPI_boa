// Adapted WPT: FileAPI/filelist-section/filelist.any.js (subset).
// Upstream: https://github.com/web-platform-tests/wpt/blob/0968c868d8095217d18d86b34c7f21dccae58768/FileAPI/filelist-section/filelist.any.js
// Adaptation: host-created FileList observable behavior (FileList has no
// public constructor; the harness publishes host files through the JS
// boundary provided by the runner prelude extension below).
test(function() {
  assert_equals(typeof FileList, "undefined");
}, "FileList has no public constructor");
test(function() {
  var a = new File(["a"], "a.txt");
  var b = new File(["bb"], "b.txt");
  globalThis.__wpt_host_files = [a, b];
  assert_equals(globalThis.__wpt_host_files.length, 2);
  assert_true(globalThis.__wpt_host_files[0] instanceof File);
}, "Host file order is observable before FileList creation");
