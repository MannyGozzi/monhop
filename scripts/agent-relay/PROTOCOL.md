# Agent relay protocol (v1)

The Mac agent (Claude) and the Windows agent (Astra) coordinate MonHop work over the LAN. A message arrives in under a
second and either side can block until the other speaks.

## Transport

- `relay.py` runs on the Mac and listens on port 24880 at the address in `host.txt` only (the Mac's LAN
  address, never committed). It accepts connections from that address, `127.0.0.1` and the peer addresses
  listed one per line in `peers.txt` (never committed) and refuses everything else. Each client keeps the
  Mac's address in its own `host.txt` next to the script.
- The token in `token.txt` next to the script (generated at first start, never committed, copied once
  to the other computer by hand) prefixes every path: `/<token>/<action>`.
- `POST send?from=mac|win` with a `text/plain; charset=utf-8` body (or JSON `{"from","text"}`) appends a
  message and returns `{"seq": N}`. Bodies up to 256 KB.
- `GET wait?since=N&timeout=S` blocks until a message with `seq > N` exists or `S` seconds pass (cap
  1500) and returns the JSON array of those messages. `GET log?since=N` is the same without blocking.
  `GET health` returns `{"seq", "now"}`.
- A message is `{"seq", "at", "from", "text"}`. `log.jsonl` on the server is append-only; `seq` is the
  global order.

## Clients

- Mac: `relay.sh send <file>` (or `-` for stdin), `relay.sh wait [timeout]`, `relay.sh log [since]`,
  `relay.sh health`. `watch.sh` streams Windows messages continuously for the Mac agent's monitor.
- Windows: `relay.ps1 -Action send -File msg.txt` (or `-Text '...'`), `relay.ps1 -Action wait [-Timeout 120]`,
  `relay.ps1 -Action log`, `relay.ps1 -Action health`. Copy the Mac's token into
  `scripts\agent-relay\token.txt` first. `since.txt` next to the script remembers the last delivered
  `seq`, so `wait` returns only messages from the other side that have not been shown yet.

## Message format

Every message is one bracketed envelope:

```
[WIN-LKM-20260913T193000Z-123]
re: MAC-104
kind: receipt
<body>
[/WIN-LKM-20260913T193000Z-123]
```

- The id is `MAC-LKM-<UTC yyyymmddThhmmssZ>-<NNN>` or `WIN-LKM-...`; `NNN` increases per side.
- `re:` names the id being answered, or `-`.
- `kind:` is one of `ack` (read, nothing else to say), `pull` (a commit to pull and reinstall),
  `receipt` (installed build, SHA-256, pid, startup log lines), `lane` (claim or release of files, listed),
  `hold` / `release` (main pushes paused or resumed), `ask` (questions numbered Q1..), `answer`
  (A1.. matching the questions), `log` (a verbatim, unfiltered excerpt with its time window),
  `drop` (a "session over" line plus the two seconds before it), `note` (anything else).
- One message per intent, self-contained: never "see above" without the id. Commit ids have at least
  nine characters. Log excerpts are verbatim and complete for the stated window.
- Never send credentials, tokens, key material, or typed text.

## Turn-taking

- Send, then `wait` whenever the next step depends on the other side; the wait blocks, so nobody polls.
  Between independent work steps, drain the inbox with one short `wait` (`-Timeout 5`).
- Every message gets at least an `ack` in the other side's next turn.
- Main pushes: `hold` before a rebase-and-push window, `release` after; the other side does not push in
  between.
