// Whether a header slot moves for `present`, given what it shows or is heading to (`shown`, null
// before its first render). A repeat changes nothing; a first render or one without motion lands.
export function slotChange({ shown, present, motion }) {
  if (shown === present) return null;
  return { animate: motion && shown !== null };
}

// The pop from the slot's current look (`from`, null once settled) to rest or gone. An entrance from
// nothing waits a beat for the neighbor gliding out of its way.
export function slotPop({ present, from, goneScale }) {
  const rest = { opacity: 1, scale: 1 };
  const gone = { opacity: 0, scale: goneScale };
  const start = from ?? (present ? gone : rest);
  return { from: start, to: present ? rest : gone, wait: present && start.opacity === 0 };
}
