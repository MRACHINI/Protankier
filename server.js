import http from "http";
import { readFileSync, writeFileSync, existsSync, mkdirSync } from "fs";
import { spawn } from "child_process";
import path from "path";
import { fileURLToPath } from "url";
import { Server } from "socket.io";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const PORT = Number(process.env.PORT || 3005);
const DATA_DIR = process.env.DATA_DIR || path.join(__dirname, "data");
const CONFIG_PATH = path.join(DATA_DIR, "config.json");
const BINARY = process.env.SERVER_CLICK_BIN || path.join(__dirname, "bin", "server-click");
const APP_VERSION = process.env.APP_VERSION || "dev";

const DEFAULT_CONFIG = {
  target: "146.59.110.195:1337",
  proxy: "",
  language: "en",
  logProtocol: false,
  joinOnLeave: true,
  retryMs: 500,
  sessionTimeoutSec: 60,
  accounts: [],
};

const DEFAULT_ACCOUNT = { username: "", password: "", battleId: "", side: "J", enabled: true };

const processes = new Map();
const accountStates = new Map();
const logs = [];
const MAX_LOGS = 500;

function loadConfig() {
  if (!existsSync(CONFIG_PATH)) return { ...DEFAULT_CONFIG };
  try {
    return { ...DEFAULT_CONFIG, ...JSON.parse(readFileSync(CONFIG_PATH, "utf8")) };
  } catch (error) {
    console.error("config.json is invalid, using defaults:", error.message);
    return { ...DEFAULT_CONFIG };
  }
}

let config = loadConfig();

function saveConfig(next) {
  mkdirSync(DATA_DIR, { recursive: true });
  writeFileSync(CONFIG_PATH, JSON.stringify(next, null, 2));
  config = next;
}

// Accepts a bare battle id or a pasted site link such as "#/battle/e2e702ee47fa9be8".
function normalizeBattleId(input) {
  const raw = String(input ?? "").trim();
  let candidate = raw;
  const anchor = raw.indexOf("#/battle/");
  const path = raw.indexOf("/battle/");
  if (anchor >= 0) candidate = raw.slice(anchor + "#/battle/".length);
  else if (path >= 0) candidate = raw.slice(path + "/battle/".length);
  candidate = candidate.split(/[?#/&\s]/)[0].trim();
  const isId = candidate.length >= 8 && candidate.length <= 32 && /^[0-9a-fA-F]+$/.test(candidate);
  // Keep the raw text when we cannot parse it, so the user sees what they typed.
  return isId ? candidate : raw;
}

function sanitizeConfig(input) {
  const out = { ...DEFAULT_CONFIG };
  out.target = String(input.target ?? DEFAULT_CONFIG.target).trim();
  out.proxy = String(input.proxy ?? "").trim();
  out.language = String(input.language ?? "en").trim() || "en";
  out.logProtocol = Boolean(input.logProtocol);
  out.joinOnLeave = Boolean(input.joinOnLeave);
  out.retryMs = Math.max(50, Number(input.retryMs) || DEFAULT_CONFIG.retryMs);
  out.sessionTimeoutSec = Math.max(5, Number(input.sessionTimeoutSec) || DEFAULT_CONFIG.sessionTimeoutSec);
  out.accounts = Array.isArray(input.accounts)
    ? input.accounts.map((a) => ({
        username: String(a.username ?? "").trim(),
        password: String(a.password ?? ""),
        battleId: normalizeBattleId(a.battleId),
        side: ["Alpha", "Bravo", "J"].includes(a.side) ? a.side : "J",
        enabled: a.enabled !== false,
      })).filter((a) => a.username)
    : [];
  return out;
}

function record(entry) {
  const line = { time: new Date().toISOString(), ...entry };
  logs.push(line);
  if (logs.length > MAX_LOGS) logs.shift();
  io.emit("log", line);
}

const MASK = "********";

function publicConfig(cfg) {
  return { ...cfg, accounts: cfg.accounts.map((a) => ({ ...a, password: a.password ? MASK : "" })) };
}

function startAccount(account, accountCount, accountIndex) {
  const key = account.username.toLowerCase();
  if (processes.has(key)) return { ok: false, error: "already running" };

  const args = [
    "--action", "click",
    "--username", account.username,
    "--password", account.password,
    "--map", account.battleId,
    "--side", account.side,
    "--target", config.target,
    "--language-code", config.language,
    "--retry-ms", String(config.retryMs),
    "--accounts", String(accountCount || 1),
    "--account-index", String(accountIndex || 0),
    "--session-timeout-sec", String(config.sessionTimeoutSec),
  ];
  if (config.proxy) args.push("--proxy", config.proxy);
  if (config.logProtocol) args.push("--log-protocol");
  if (config.joinOnLeave) args.push("--join-on-leave");

  const child = spawn(BINARY, args, { stdio: ["ignore", "pipe", "pipe"] });
  processes.set(key, child);
  accountStates.set(key, { phase: "starting", detail: "", updated: Date.now() });
  io.emit("status", { username: account.username, status: "running" });
  io.emit("state", { username: account.username, phase: "starting", detail: "" });
  record({ level: "info", username: account.username, message: "started" });

  const onData = (buf) => {
    for (const raw of buf.toString().split("\n")) {
      const message = raw.trim();
      if (!message) continue;

      /* The binary reports structured state on its own line, separate from the log text. */
      if (message.startsWith("STATE ")) {
        const [phase, detail = ""] = message.slice(6).split("\t");
        accountStates.set(key, { phase, detail, updated: Date.now() });
        io.emit("state", { username: account.username, phase, detail });
        if (phase === "rank") {
          const previous = config.accounts.find((a) => a.username.toLowerCase() === key);
          const rank = Number(detail);
          if (previous && Number.isFinite(rank) && previous.rank !== rank) {
            previous.rank = rank;
            saveConfig(config);
          }
        }
        continue;
      }

      record({ level: "log", username: account.username, message: message.replace(account.password, "***") });
    }
  };
  child.stdout.on("data", onData);
  child.stderr.on("data", onData);

  child.on("error", (error) => {
    record({ level: "error", username: account.username, message: `failed to start: ${error.message}` });
  });

  child.on("exit", (code, signal) => {
    processes.delete(key);
    /* Keep the specific reason the binary reported (battle gone, taken over, failed) rather
       than flattening every non-zero exit into a generic "stopped". */
    const previous = accountStates.get(key) || {};
    const keepReason = previous.phase === "battle-gone" || previous.phase === "failed" || previous.phase === "taken-over";
    if (!keepReason) {
      accountStates.set(key, { phase: "stopped", detail: `code=${code}`, updated: Date.now() });
      io.emit("state", { username: account.username, phase: "stopped", detail: `code=${code}` });
    }
    io.emit("status", { username: account.username, status: "stopped" });
    record({ level: code === 0 ? "info" : "error", username: account.username, message: `exited code=${code} signal=${signal}` });
  });

  return { ok: true };
}

// Fetches the live battle list by logging in with an idle account.
function fetchBattles(account) {
  return new Promise((resolve) => {
    const args = [
      "--action", "list",
      "--username", account.username,
      "--password", account.password,
      "--target", config.target,
      "--language-code", config.language,
    ];
    if (config.proxy) args.push("--proxy", config.proxy);

    const child = spawn(BINARY, args, { stdio: ["ignore", "pipe", "pipe"] });
    let out = "";
    let err = "";
    const timer = setTimeout(() => {
      child.kill();
      resolve({ ok: false, error: "timed out fetching the battle list" });
    }, 25000);

    child.stdout.on("data", (d) => (out += d.toString()));
    child.stderr.on("data", (d) => (err += d.toString()));

    child.on("exit", () => {
      clearTimeout(timer);
      const rankLine = out.split("\n").find((l) => l.startsWith("RANK\t"));
      const battlesLine = out.split("\n").find((l) => l.startsWith("BATTLES\t"));
      if (!battlesLine) {
        const cleaned = err.replace(account.password, "***").split("\n").filter(Boolean).slice(-3).join(" ");
        return resolve({ ok: false, error: cleaned || "no battle list returned" });
      }
      try {
        const payload = JSON.parse(battlesLine.slice(8));
        const rank = rankLine ? Number(rankLine.slice(5)) : null;
        resolve({ ok: true, rank, battles: payload.battles || [] });
      } catch (error) {
        resolve({ ok: false, error: `could not parse battle list: ${error.message}` });
      }
    });
  });
}

function stopAccount(username) {
  const key = String(username).toLowerCase();
  const child = processes.get(key);
  if (!child) return { ok: false, error: "not running" };
  child.kill();
  return { ok: true };
}

function runningList() {
  return [...processes.keys()];
}

const UI_PATH = path.join(__dirname, "public", "index.html");

const server = http.createServer((req, res) => {
  const url = new URL(req.url, `http://${req.headers.host}`);

  if (req.method === "GET" && url.pathname === "/health") {
    res.writeHead(200, { "Content-Type": "application/json" });
    return res.end(JSON.stringify({ ok: true, version: APP_VERSION, running: runningList().length }));
  }

  if (req.method === "GET" && (url.pathname === "/" || url.pathname === "/index.html")) {
    res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
    return res.end(readFileSync(UI_PATH));
  }

  if (req.method === "GET" && url.pathname === "/api/battles") {
    /* Listing logs in, so never use an account that is currently clicker-active: that would
       evict its own session. Prefer an idle enabled account. */
    const idle = config.accounts.find(
      (a) => a.enabled && a.username && !processes.has(a.username.toLowerCase())
    );
    if (!idle) {
      res.writeHead(409, { "Content-Type": "application/json" });
      return res.end(
        JSON.stringify({
          ok: false,
          error: "all enabled accounts are running; stop one to browse battles",
        })
      );
    }

    fetchBattles(idle).then((result) => {
      if (result.ok && result.rank != null) {
        const stored = config.accounts.find((a) => a.username.toLowerCase() === idle.username.toLowerCase());
        if (stored && stored.rank !== result.rank) {
          stored.rank = result.rank;
          saveConfig(config);
        }
      }
      res.writeHead(result.ok ? 200 : 502, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ ...result, via: idle.username }));
    });
    return;
  }

  if (req.method === "GET" && url.pathname === "/api/config") {
    res.writeHead(200, { "Content-Type": "application/json" });
    return res.end(JSON.stringify(publicConfig(config)));
  }

  if (req.method === "PUT" && url.pathname === "/api/config") {
    let body = "";
    req.on("data", (chunk) => (body += chunk));
    req.on("end", () => {
      try {
        const next = sanitizeConfig(JSON.parse(body || "{}"));
        for (const account of next.accounts) {
          const previous = config.accounts.find((a) => a.username.toLowerCase() === account.username.toLowerCase());
          // A blank or still-masked field means "unchanged"; never persist the mask itself.
          if ((!account.password || account.password === MASK) && previous) account.password = previous.password;
        }
        saveConfig(next);
        record({ level: "info", message: "configuration saved" });
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ ok: true, config: publicConfig(config) }));
      } catch (error) {
        res.writeHead(400, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ ok: false, error: error.message }));
      }
    });
    return;
  }

  res.writeHead(404, { "Content-Type": "application/json" });
  res.end(JSON.stringify({ ok: false, error: "not found" }));
});

const io = new Server(server, {
  cors: { origin: "*", methods: ["GET", "POST", "PUT"] },
});

io.on("connection", (socket) => {
  socket.emit("hello", {
    running: runningList(),
    logs: logs.slice(-200),
    states: Object.fromEntries(accountStates),
  });

  socket.on("start", (username, ack) => {
    const account = config.accounts.find((a) => a.username.toLowerCase() === String(username).toLowerCase());
    if (!account) return ack?.({ ok: false, error: "unknown account" });
    const enabled = config.accounts.filter((a) => a.enabled);
    const index = enabled.findIndex((a) => a.username.toLowerCase() === account.username.toLowerCase());
    ack?.(startAccount(account, enabled.length, Math.max(0, index)));
  });

  // Stagger starts so the accounts never hit the game server at the same instant.
  // Each account gets its own slot inside the retry window, so their retry loops
  // interleave instead of colliding every cycle.
  socket.on("start-all", async (_, ack) => {
    const targets = config.accounts.filter((a) => a.enabled);
    const step = Math.max(50, Math.floor(config.retryMs / Math.max(1, targets.length)));
    const results = [];
    for (let i = 0; i < targets.length; i++) {
      const account = targets[i];
      results.push({ username: account.username, ...startAccount(account, targets.length, i) });
      if (i < targets.length - 1) {
        io.emit("progress", { done: i + 1, total: targets.length });
        await new Promise((resolve) => setTimeout(resolve, step));
      }
    }
    ack?.({ ok: true, results, stepMs: step, periodMs: Math.max(1200, config.retryMs) });
  });

  socket.on("stop", (username, ack) => ack?.(stopAccount(username)));

  socket.on("stop-all", (_, ack) => {
    for (const key of [...processes.keys()]) stopAccount(key);
    ack?.({ ok: true });
  });
});

function shutdown() {
  for (const child of processes.values()) child.kill();
  server.close(() => process.exit(0));
}
process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);

server.listen(PORT, "0.0.0.0", () => {
  console.log(`ProTanki clicker UI on http://0.0.0.0:${PORT} (${APP_VERSION})`);
});
