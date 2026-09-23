// The first of `candidates` the engine can parse as an easing, so Web Animations never throws on a
// token it does not know: linear() reached WebKit in Safari 17.2, and macOS 14.0 ships 17.0.
export function usableEasing(candidates, supports) {
  return candidates.find((value) => Boolean(value) && supports(value)) ?? "ease-out";
}
