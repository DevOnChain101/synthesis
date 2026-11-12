import test from "node:test";
import assert from "node:assert/strict";
import { leaks, mentionedIn, FEATURE } from "./check-release-features.mjs";

const meta = (over = {}) => ({
  packages: [
    {
      name: "synthesis-desktop",
      features: { default: [], shot: ["gpui/test-support"], [FEATURE]: [], ...(over.features ?? {}) },
      dependencies: [{ name: "synthesis-desktop", kind: "dev", features: [FEATURE] }],
    },
    { name: "other", features: {}, dependencies: over.otherDeps ?? [] },
  ],
});

test("the self dev-dependency is the only way in and is fine", () => {
  assert.deepEqual(leaks(meta()), []);
});

test("a default feature that pulls it in is reported, also through another feature", () => {
  assert.equal(leaks(meta({ features: { default: [FEATURE] } })).length, 1);
  assert.equal(leaks(meta({ features: { default: ["shot"], shot: ["dep:image", FEATURE] } })).length, 1);
});

test("a normal or build dependency that enables it is reported, a dev one is not", () => {
  const dep = (kind) => [{ name: "synthesis-desktop", kind, features: [FEATURE] }];
  assert.equal(leaks(meta({ otherDeps: dep(null) })).length, 1);
  assert.equal(leaks(meta({ otherDeps: dep("build") })).length, 1);
  assert.deepEqual(leaks(meta({ otherDeps: dep("dev") })), []);
});

test("a missing crate fails closed", () => {
  assert.equal(leaks({ packages: [] }).length, 1);
});

test("a workflow naming the feature is reported", () => {
  assert.deepEqual(mentionedIn({ "a.yml": "run: cargo build", "b.yml": `cargo build --features ${FEATURE}` }), ["named in b.yml"]);
});
