import assert from "node:assert/strict";
import test from "node:test";

import {
  beginCopyFeedback,
  copyFeedbackFailure,
  copyFeedbackFor,
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

test("copyFeedbackFor hides feedback once the subject it targeted has changed", () => {
  const feedback = copyFeedbackSuccess("code-1", 1, "Copied.");
  assert.equal(copyFeedbackFor(feedback, "code-1"), feedback);
  assert.deepEqual(copyFeedbackFor(feedback, "code-2"), emptyCopyFeedback());
  assert.deepEqual(copyFeedbackFor(feedback, null), emptyCopyFeedback());
});

test("copyFeedbackFor compares object subjects field by field", () => {
  const subject = { localCode: "abc", generation: 3 };
  const feedback = beginCopyFeedback(subject, 5);
  assert.equal(copyFeedbackFor(feedback, { localCode: "abc", generation: 3 }), feedback);
  assert.deepEqual(
    copyFeedbackFor(feedback, { localCode: "abc", generation: 4 }),
    emptyCopyFeedback(),
  );
  assert.deepEqual(
    copyFeedbackFor(feedback, { localCode: "xyz", generation: 3 }),
    emptyCopyFeedback(),
  );
});

test("a stale reply is dropped once a newer request has started for the same subject", () => {
  // The user copied twice in a row: the first reply must not stomp on the second attempt's state.
  assert.equal(isCopyReplyCurrent(1, 1, "code-1", "code-1"), true);
  assert.equal(isCopyReplyCurrent(1, 2, "code-1", "code-1"), false);
});

test("a stale reply is dropped once the subject changed underneath it", () => {
  // Pairing generation bumped (a new pairing operation started) or the code itself changed.
  const subject = { localCode: "code-1", generation: 1 };
  assert.equal(isCopyReplyCurrent(1, 1, subject, { localCode: "code-1", generation: 1 }), true);
  assert.equal(isCopyReplyCurrent(1, 1, subject, { localCode: "code-1", generation: 2 }), false);
  assert.equal(isCopyReplyCurrent(1, 1, subject, { localCode: "code-2", generation: 1 }), false);
});

test("a primitive subject (the drop copy's text) compares by plain equality", () => {
  assert.equal(isCopyReplyCurrent(1, 1, "failed: boom", "failed: boom"), true);
  assert.equal(isCopyReplyCurrent(1, 1, "failed: boom", "a different failure"), false);
  assert.equal(isCopyReplyCurrent(1, 1, "failed: boom", null), false);
});
