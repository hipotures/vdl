import test from "node:test";
import assert from "node:assert/strict";
import { normalizeURL, age } from "../src/vdl/static/url.js";

test("query stripping keeps the exact profile URL", () => {
  assert.equal(normalizeURL("  https://www.youtube.com/@test?si=a&b=2  "), "https://www.youtube.com/@test");
  assert.equal(normalizeURL("https://www.instagram.com/test/"), "https://www.instagram.com/test/");
  assert.equal(normalizeURL("https://example.test/test#section"), "https://example.test/test#section");
});
test("invalid and credential-bearing URLs cannot be submitted", () => {
  for (const url of ["", "ftp://example.test/a", "javascript:alert(1)", "file:///etc/passwd", "https://", "https:////example.test/a", "https://@example.test/a", "https://u:p@example.test/a", "https://example.test/a b", "https://example.test:bad/a", "https://example.test/\\a", "a".repeat(4097)]) {
    assert.throws(() => normalizeURL(url), undefined, url);
  }
});
test("age handles missing, recent and future timestamps", () => {
  assert.equal(age(null, 100), "Never checked");
  assert.equal(age(90, 100), "10s ago");
  assert.equal(age(101, 100), "0s ago");
  assert.equal(age(0, 3600), "1h ago");
});
