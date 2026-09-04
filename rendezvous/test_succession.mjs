import { nextMaster } from "./succession.mjs";
import assert from "node:assert/strict";

const members = [
  { id: "a", joinedAt: 1 },
  { id: "b", joinedAt: 2 },
  { id: "c", joinedAt: 3 },
];
assert.equal(nextMaster(members, "a"), "b");
assert.equal(nextMaster(members, "b"), "a");
assert.equal(nextMaster([{ id: "solo", joinedAt: 1 }], "solo"), null);
assert.equal(nextMaster([], "ghost"), null);

const tied = [
  { id: "zeta", joinedAt: 5 },
  { id: "alpha", joinedAt: 5 },
  { id: "mu", joinedAt: 5 },
];
assert.equal(nextMaster(tied, "zeta"), "alpha");
console.log("succession ok");
