// Copy-to-clipboard feedback tied to the value it copied. A reply or a scheduled reset only
// applies while its subject and request slot are still the current one; anything else is a
// stale reply and is dropped.

export function emptyCopyFeedback() {
  return { state: "idle", message: "", subject: null, request: 0 };
}

export function beginCopyFeedback(subject, request) {
  return { state: "pending", message: "Copying…", subject, request };
}

export function copyFeedbackSuccess(subject, request, message) {
  return { state: "success", message, subject, request };
}

export function copyFeedbackFailure(subject, request, message) {
  return { state: "error", message, subject, request };
}

// Whether a reply or timeout issued for (subject, request) still targets the current subject and slot.
export function isCopyReplyCurrent(request, currentRequest, subject, currentSubject) {
  return request === currentRequest && subject === currentSubject;
}
