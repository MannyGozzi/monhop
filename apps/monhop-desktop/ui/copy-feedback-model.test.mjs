import assert from "node:assert/strict";
import test from "node:test";

import {
  beginCopyFeedback,
  copyFeedbackFailure,
  copyFeedbackSuccess,
  emptyCopyFeedback,
  isCopyReplyCurrent,
} from "./copy-feedback-model.mjs";

test("empty feedback is idle with no subject or request", () => {
  assert.deepEqual(emptyCopyFeedback(), { state: "idle", message: "", subject: null, request: 0 });
});

test("begin/success/failure carry the subject and request they were issued for", () => {
  assert.deepEqual(beginCopyFeedback("abc", 1), {
    state: "pending",
    message: "Copying…",
    subject: "abc",
    request: 1,
  });
  assert.deepEqual(copyFeedbackSuccess("abc", 1, "Copied."), {
    state: "success",
    message: "Copied.",
    subject: "abc",
    request: 1,
  });
  assert.deepEqual(copyFeedbackFailure("abc", 1, "Could not copy: boom"), {
    state: "error",
    message: "Could not copy: boom",
    subject: "abc",
    request: 1,
  });
});

test("a stale reply is dropped once a newer request has started for the same subject", () => {
  // The user copied twice in a row: the first reply must not stomp on the second attempt's state.
  assert.equal(isCopyReplyCurrent(1, 1, "failed: boom", "failed: boom"), true);
  assert.equal(isCopyReplyCurrent(1, 2, "failed: boom", "failed: boom"), false);
});

test("a primitive subject (the drop copy's text) compares by plain equality", () => {
  assert.equal(isCopyReplyCurrent(1, 1, "failed: boom", "failed: boom"), true);
  assert.equal(isCopyReplyCurrent(1, 1, "failed: boom", "a different failure"), false);
  assert.equal(isCopyReplyCurrent(1, 1, "failed: boom", null), false);
});
