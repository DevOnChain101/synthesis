import test from "node:test";
import assert from "node:assert/strict";
import { leaks, mentionedIn, FEATURES } from "./check-release-features.mjs";

const defs = () => Object.fromEntries(FEATURES.map((f) => [f, []]));
const meta = (over = {}) => ({
  packages: [
    {
      name: "synthesis-desktop",
      features: { default: [], ...defs(), ...(over.features ?? {}) },
      dependencies: [{ name: "synthesis-desktop", kind: "dev", features: ["local-engine-for-tests"] }],
    },
    { name: "other", features: {}, dependencies: over.otherDeps ?? [] },
  ],
});

test("the self dev-dependency is the only way in and is fine", () => {
  assert.deepEqual(leaks(meta()), []);
});

test("a default feature that pulls a guarded one in is reported, also through another feature or a dep", () => {
  for (const f of FEATURES) {
    assert.equal(leaks(meta({ features: { default: [f] } })).length, 1, f);
    assert.equal(leaks(meta({ features: { default: ["x"], x: ["dep:image", f] } })).length, 1, f);
  }
  assert.equal(leaks(meta({ features: { default: ["x"], x: ["synthesis-desktop/shot"] } })).length, 1);
});

test("a normal or build dependency that enables one is reported, a dev one is not", () => {
  for (const f of FEATURES) {
    const dep = (kind) => [{ name: "synthesis-desktop", kind, features: [f] }];
    assert.equal(leaks(meta({ otherDeps: dep(null) })).length, 1, f);
    assert.equal(leaks(meta({ otherDeps: dep("build") })).length, 1, f);
    assert.deepEqual(leaks(meta({ otherDeps: dep("dev") })), [], f);
  }
});

test("a feature that no longer exists fails closed (a rename must not make the guard vacuous)", () => {
  for (const f of FEATURES) {
    const features = defs();
    delete features[f];
    const m = meta();
    m.packages[0].features = { default: [], ...features };
    const r = leaks(m);
    assert.equal(r.length, 1, f);
    assert.match(r[0], /not defined/);
  }
});

test("a missing crate fails closed", () => {
  assert.equal(leaks({ packages: [] }).length, 1);
});

test("workflows: another crate's feature of the same name is allowed, ours and --all-features are not", () => {
  const ok = { "ci.yml": "run: cargo test --workspace --features synthesis-cli/test-provider" };
  assert.deepEqual(mentionedIn(ok), []);
  const bad = {
    "a.yml": "run: cargo build -p synthesis-desktop --features shot",
    "b.yml": "run: cargo build --features synthesis-desktop/local-engine-for-tests",
    "c.yml": 'run: cargo build -F "x,test-provider"',
    "d.yml": "run: cargo build --all-features",
    "e.yml": "run: cargo build --features=shot",
  };
  assert.equal(mentionedIn(bad).length, 5);
  assert.deepEqual(mentionedIn({ "f.yml": "run: cargo build --release -p synthesis-desktop" }), []);
});
