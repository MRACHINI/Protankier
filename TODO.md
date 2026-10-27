# TODO / Backlog

Ideas worth checking later. Nothing here is implemented unless it says so.

Last updated after `v2.0.17`.

## UI requests from the user

### 0. Battle filter box that survives a refresh

Add a text filter above the battle picker. Typing narrows the list by battle name / id /
map. The important part: **the filter text must persist across a page refresh** (save it in
`localStorage` and restore it on load), so a reload does not wipe what the user was
searching for. Same for the rank filter if it is a text field rather than a fixed dropdown.

Not implemented yet.

### 0b. Codecs that under-read their payload

The repaired leftover check now reports exactly which layouts are incomplete. Measured over
4 live sessions:

```
id=-64696933   x5938  left 4    TankMoveCommand
id=1516578027  x2800  left 4    TankMoveTurretCommand
id=-44282936   x600   left 8    (model 51 twin shot)
id=-369590613  x48    left variable  (model 54 hit)
id=1422563374  x4     left 5    FriendsInitialize
```

Harmless today (the extra bytes are skipped, the packet still parses), but each one means
real data is being dropped, and a 4-byte under-read at a different offset would become a
hard decode failure. All four are `Vector3<f32>`-shaped gaps of 4 or 8 bytes.

Not urgent. Fix when a packet actually needs the missing field.

## Open questions

### 1. Unexplained disconnects (highest priority) — investigation in progress

Originally described as "a clean disconnect every ~72s". Measured lifetimes since then are
much more variable than that, so the fixed-timer theory is dead:

```
76.6s  80.2s  87.4s  71.6s  51.2s  37.4s  30.4s(evicted)  15.4s  13.2s  11.1s
405.2s  300s+ (survived)   0.6s
```

**Ruled out by measurement (v2.0.17, full `--log-protocol` traces):**

- **Idle timeout.** No. Traffic is continuous while sitting in a battle, and the close
  happens 0.1–0.2s after a normal packet, right after a `ServerSessionSync` we answered.
- **Keepalive not being answered.** No. `PingMeasurePing` arrives every ~1s and our
  `PingMeasurePong` goes back in 0–1ms (measured over 150 pings: min 0, p50 0, p95 0,
  max 1ms). `ServerSessionSync` arrives every ~2s and is answered every time.
  `LowLevelPing` and `SessionPing` are both registered in `connect()`.
- **The old `TimeChecker` anti-cheat suspect.** No. That model does not exist in this
  protocol table at all; it was from the older reference client.
- **Battle-specific.** No. Drops also happen before ever reaching a battle.
- **Fixed server-side session lifetime.** No. Variance is 0.6s to 405s.

**Superseded: the "rate/cooldown" reading.** An earlier pass read the variance as the
server punishing rapid re-logins:

```
after 240s of no connections : 405.2s
after ~10s gap               : 11.1s
back-to-back test runs       : 0.6s, 13.2s, 15.4s
```

That correlation is most likely coincidental. Under the decode-failure explanation the
lifetime is simply how long until a specific packet happens to arrive, and a battle with
more players produces those packets sooner. Do not build a cooldown on this until the
packet theory is confirmed by the capture described below.

**Both disconnect kinds occur:** `ConnectionClosed(Disconnected)` (clean EOF) and
`IOError(UnexpectedEof)`. Both sides of a takeover see the *same*
`ConnectionClosed(Disconnected)`, so the error type cannot distinguish eviction from a
server drop — this is why the app now never reconnects on its own.

**`IOError(UnexpectedEof)` is NOT a network drop (corrected).** Socket read and write
errors are mapped to `ConnectionClosed(ReadError)` / `ConnectionClosed(WriteError)` in
`protocol/src/connection.rs` (lines 268 and 282), so a real socket failure never produces
`ProtocolError::IOError`. The only remaining source of `IOError(Eof)` is our *own decoder*
running out of bytes inside a frame, i.e.:

```
packet frame arrives (correct length, correct id)
  -> packet_registry.decode(...)
  -> some codec_struct! field decode hits read_exact past the end
  -> io::Error(UnexpectedEof) -> ProtocolError::IOError
  -> try_parse_read_buffer returns Err
  -> connection treated as dead -> whole session ends
```

So every "abrupt drop" is really **a packet we failed to decode**, not the server closing
us. That also explains the wild variance (0.6s to 405s): it depends on which packets the
server happens to send, not on elapsed time or on how recently we logged in. A battle full
of activity is far more likely to deliver the offending packet early.

Most likely writer of such a packet: one of the ~30 optionality/width mismatches still
listed in section 4 below. Any packet we mis-layout kills the session the first time it
arrives.

**Two consequences.**

1. A single undecodable packet must not be fatal. The frame length is known from the
   header, so the bad frame can be skipped, logged with its id and byte count, and the
   session continued. Framing errors (bad header, oversize) stay fatal.
2. The offending packet id is directly observable. `FROST_TRACE_FRAMES=1` prints
   `[FRAME] len=... id=...` *before* decoding, so the frame immediately before the error
   names the culprit.

**Next experiment: superseded.** The cooldown idea was dropped once the decode mechanism was
found; lifetime variance is better explained by *which* packets arrive.

**STATUS: codec fix built as `test-codec`, not yet released.**

The inverted leftover-bytes check was repaired, and it named the offenders; 22 reachable
mismatches were then corrected against the reference metadata. The two packets that still
fail are `X2X` broadcasts (`522993449`, `-1959138292`) whose `initParams` type is absent from
the packet table, so they are correctly skipped and logged.

**What is proven:** before the fix, a decode failure killed the session outright; after it,
no session has died with a decode error (`decode-deaths 0/3` in the A/B below). One
remaining layout warning at login (`FriendsInitialize`, id `1422563374`) is logged and
harmless.

**What is NOT proven:** that the fix removes the *clean closes*. A 3-run-per-build A/B gave:

```
v2.0.17   lives 180, 180, 10   decode-deaths 0/3
test-codec lives 180,  60, 180  decode-deaths 0/3
```

Both builds survived 2 of 3 runs, so **the two are indistinguishable at n=3** and the clean
close is a separate, still-unexplained failure mode. It is not a decode error. Candidates:
the battle ending/restarting (normal behaviour), a server-side close, or a missing
client-to-server packet such as `Ready2Place` / `Ready2Activate`.

Do not claim the drops are fixed until the tail-capture experiment below says why the clean
close happens.

Remaining unexplained failure: an occasional **clean close** — now explained.

**The clean close is the battle ending, not a fault.** A tail capture of the last packets
before each close shows battle-lifecycle traffic immediately beforehand:

```
run 1  240s  survived   ... TankUpdateTemperature -> Ping
run 2   45s  clean      ... BattleInfoRemoveUser -> BattleUserList* -> BattleListBattleSelect
                            -> BattleInfoInfoDestroy -> BattleInfoRoundFinish -> CLOSE
run 3  240s  survived   ... TankActivated -> Ping -> ServerSessionSync
run 4  240s  survived   ... TankMoveTurretCommand -> TankUpdateTemperature
```

`BattleInfoInfoDestroy` + `BattleInfoRoundFinish` mean the round finished and the server
destroyed the battle. The socket close is the server tearing the battle down, so the
session ends through no fault of ours. That is also why these clusters are short (~45s)
and clustered around round ends.

**Consequence for the reconnect policy.** The earlier conclusion "no signal distinguishes
an eviction from a server close" is now partly wrong: a close that follows
`BattleInfoRoundFinish` / `BattleInfoInfoDestroy` *is* identifiable as "the battle ended".
Reconnecting in that case cannot kick a player, because the account was not taken over.
Whether to act on that is a product decision (see open question below), not a technical one.

### 2. Duplicate-session detection

See "Detect an existing session from another location" below.

### 3. `--retry-ms` floor is 1.2s, not the requested 500ms

Each failed attempt costs ~300ms and the session cannot drain the packets it generates
(battle list, battle info, chat) before the next attempt is due. Below ~1.2s the gap
drifts upward without bound. Measured exact at 1.5s and 2.0s, unusable at 0.5s.

Possible fix: reuse the cached battle list instead of re-requesting it each attempt,
which should make an attempt much cheaper. Risk: acting on a stale list.

## Remaining work

### 4. Codec optionality mismatches — fixed, keep the checker

All 30 mismatches that the reflection tool can see were corrected in `v2.0.18`. The tool
had been reporting them for a while; what made them matter was that one undecodable packet
used to end the whole session.

The connection layer now logs two things that make future drift obvious:

- `Skipped undecodable packet id N (<len> bytes): <reason>` — a packet we could not parse.
- `Packet id N decoded X of Y bytes (Z bytes left over; codec layout may be out of date).`
  — a packet that parsed but whose layout is missing a field. This is the early warning that
  precedes a hard failure.

The leftover check used to be inverted (it fired only when a packet consumed every byte), so
it never reported anything. That is now correct.

Re-run the diff at any time:

```
dotnet run --no-build -- <ProtankiNetworking.dll> <net8.0 runtime dir> protocol/src/codec/structs.rs
```

Append a struct name as a fourth argument to dump the reference field layout for that one
struct, which is how a specific decode failure gets diagnosed.

### 5. No automated tests

Everything so far was validated by running against the live production server with a real
account. That is slow, risky and pollutes player-facing state. See "Local mock server".

### 6. Unverified paths

- **Real leave -> rejoin.** No player actually left during any test window, so the
  slot-sniping path has never completed a full cycle against the live server.
- **Proxy mode** (`--proxy`) has not been exercised since the join rewrite.
- **Multi-account** could not be tested end-to-end: two concurrent sessions of the same
  account evict each other, so a second test account is needed.

### 7. Repository and operational housekeeping

- 10 release images on production (`v2.0.5` .. `v2.0.14`). Consider pruning all but the
  last two or three.
- `main` is still behind `rewrite/protanki-clicker-v2`. Intentional for now: old code
  stays the recoverable default. Decide when to fast-forward.
- The UI server keeps config in memory; editing `/opt/protankier-data/config.json` behind
  its back is silently overwritten. Always change config through the UI or
  `PUT /api/config`.
- `retryMs`, `sessionTimeoutSec` and the new battle-filter fields are saved globally, not
  per account.

## Detect an existing session from another location

**Problem.** If the account is already logged in elsewhere (the player opens the real
game client), the server evicts whichever session logged in first. Our reconnects then
evict the player, the player reconnects, and the two fight in a loop. `v2.0.13` added a
takeover counter that stops after 4 rounds, but that means the clicker has to kick the
player a few times before giving up.

**Question to answer.** After a successful connect, can the client tell that this account
is *already* connected from another location, so it can refuse to start (or back off)
instead of evicting the real player?

**Where to look.**

- `ItemJoinedTeamBattleInPacket` / `ItemJoinedDmBattleInPacket` (model 32) announce
  membership changes. Whether they are broadcast to all sessions of the same account, or
  only to the session in the battle, is unknown.
- `OnlineStatusInPacket` / `InBattleStatusInPacket` / `NotInBattleStatusInPacket`
  (model 18) look like presence notifications and may carry our own user id.
- The login sequence may already answer this: check whether anything after
  `AccountLoginSuccess` differs when the account is logged in twice.
- Compare packet flow of a single session against two concurrent sessions of the same
  account and diff the traffic.

**Cheap experiment.** Log in twice with the same account from two clients on the same
host, log every incoming packet on both, and diff. If the server tells either side that
another session exists, that packet is the answer.

**If no such signal exists.** Fall back to a heuristic: after login, if the session dies
with a clean disconnect within N seconds with no battle activity, treat it as a takeover
and stay down with a long cool-off rather than retrying after 1s. A second safety net is a
manual lock in the UI ("I am playing now, do not start") that the clicker respects.

## Local mock server

**Why.** Right now the only way to verify a change is to run it against the live server
with a real account. That is how the `ChatCC` bug survived so long, and how I ended up
kicking a real player's session during testing.

**How.** `protanki-protocol-rs` already ships a working mock server
(`server/src/server.rs`, `client_components/*`) that speaks this exact protocol,
including `ClientBattleList` which pushes a battle list with a configurable dummy battle.
Plan:

1. Port the minimal subset into `applications/mock-server` in this repo: handshake,
   resource acks, login, lobby switch, battle list, select, join, and a "battle is full"
   path.
2. Make battle occupancy configurable so the full -> slot-free transition can be scripted.
3. Add integration tests that drive `server-click` against it:
   - joins a listed battle
   - retries when full, then joins when a slot frees
   - stops on wrong credentials
   - stops on a battle id missing from the list
   - recognises a takeover disconnect and stands down
4. Optionally reuse the same server for manual UI testing without touching production.

**What it buys.** Fast, repeatable, offline verification of every behaviour above,
including the paths that have never been exercised (real leave -> rejoin, multi-account
staggering with two distinct accounts).

## Suggested features (not implemented)

Ranked by expected value.

**High**

- ~~Per-account state display~~ — implemented in `v2.0.15`.
- ~~Battle resolver / preview~~ — implemented in `v2.0.15`.
- ~~Battle-gone watchdog~~ — implemented in `v2.0.15`.
- ~~Battle picker~~ — implemented in `v2.0.15`.
- ~~Audio notifications~~ — implemented in `v2.0.15`.
- Verbose log toggle, so `[UNKNOWN-IN]` hex dumps can be hidden. Still open.

**Medium**

- Desktop/Telegram/Discord notifications for joined, taken over, credentials rejected and
  battle disappeared. Audio covers the basics; remote notification would matter if the
  app is watched from another machine.
- Per-account proxy. `--proxy` is global today; multi-account from one IP is the obvious
  way to get flagged.
- Surface permanent failures (wrong password, banned, battle not listed) as a red banner
  on the account row instead of `exited code=1` buried in the log.

**Lower / speculative**

- Join statistics: attempts per minute and time-to-slot, useful for tuning the retry
  floor honestly.
- Scheduled start/stop windows.
- Spectate-only mode for rank-restricted battles.
