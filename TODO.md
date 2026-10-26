# TODO / Backlog

Ideas worth checking later. Nothing here is implemented unless it says so.

Last updated after `v2.0.14`.

## Open questions

### 1. Unexplained ~72s disconnect (highest priority)

Every session observed on the live server ended with a clean server disconnect roughly
72–76 seconds after connecting, regardless of activity:

```
13:05:19 connected -> 13:05:48 joined -> 13:06:31 disconnect   (72s)
23.4s   connected -> 24.0s   joined   -> 99.2s   disconnect     (76s)
```

The reconnect loop hides it, but it means we re-login every ~72s while sitting in a
battle. That is churn on the game server and may itself provoke the takeover fights.
Cause unknown.

Prime suspect: the real client answers a periodic anti-cheat ping (`TimeChecker` model,
`TimeCheckerPongOutPacket`) which we never send. Other candidates are the session sync
(`ServerSessionSync` / `ServerSessionSyncResponse`) or a kick-for-inactivity timer.

**Next step (owner: user).** Play the real game client and note whether it also
disconnects about every ~75 seconds. If the real client stays connected, the cause is a
packet we are not sending. If it also drops, it is normal server behaviour and only our
reconnect timing needs tuning.

### 2. Duplicate-session detection

See "Detect an existing session from another location" below.

### 3. `--retry-ms` floor is 1.2s, not the requested 500ms

Each failed attempt costs ~300ms and the session cannot drain the packets it generates
(battle list, battle info, chat) before the next attempt is due. Below ~1.2s the gap
drifts upward without bound. Measured exact at 1.5s and 2.0s, unusable at 0.5s.

Possible fix: reuse the cached battle list instead of re-requesting it each attempt,
which should make an attempt much cheaper. Risk: acting on a stale list.

## Remaining work

### 4. 30 codec optionality mismatches unfixed

`ChatCC.min_word` (i16), `ResourceReference.resource_id` (u64) and `BattleMine.position`
(non-optional) were fixed because they broke the join. Scoreboard widths (`UserInfo`,
`UserStat`, `BattleInfoUser`) were fixed too. Roughly 30 structs remain where our
`Option<T>` disagrees with the reference `Nullable<T>`, for example:

- `MoveCommand.Position` / `Orientation` / velocities — ours optional, really plain
- `UserInfo.ChatModeratorLevel`, `UserStatus.ChatModeratorLevel` — ours plain, really optional
- `BattleInfoData.Mode`, `StatisticsModelCC.BattleMode` — ours plain, really optional
- `ChatMessage.SourceUser` / `TargetUser` — ours optional, really plain
- `UserContainerCC.Users`, `ClientPointData.TankIds` — array optionality differs

These are latent: the packets are currently ignored, so they have not hurt. Any of them
will corrupt a packet the moment we start parsing that model.

The full diff is reproducible by reflecting over the MIT-licensed `ProtankiNetworking`
3.0.1 NuGet package and comparing `[Encode(n)]` fields against `codec_struct!` in
`protocol/src/codec/structs.rs`.

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
