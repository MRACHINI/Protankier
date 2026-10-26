# TODO / Backlog

Ideas worth checking later. Nothing here is implemented yet.

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
and stay down for a long cool-off rather than reconnecting after 1s.

**Related.** `--retry-ms` floor is currently 1.2s because a failed attempt costs ~300ms
and the session cannot drain packets faster. Revisit if attempt checks are made cheaper
(reuse the cached battle list instead of re-requesting it each time).
