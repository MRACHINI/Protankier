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
        battleId: String(a.battleId ?? "").trim(),
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

function publicConfig(cfg) {
  return { ...cfg, accounts: cfg.accounts.map((a) => ({ ...a, password: a.password ? "********" : "" })) };
}

function startAccount(account) {
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
    "--session-timeout-sec", String(config.sessionTimeoutSec),
  ];
  if (config.proxy) args.push("--proxy", config.proxy);
  if (config.logProtocol) args.push("--log-protocol");
  if (config.joinOnLeave) args.push("--join-on-leave");

  const child = spawn(BINARY, args, { stdio: ["ignore", "pipe", "pipe"] });
  processes.set(key, child);
  io.emit("status", { username: account.username, status: "running" });
  record({ level: "info", username: account.username, message: "started" });

  const onData = (buf) => {
    for (const raw of buf.toString().split("\n")) {
      const message = raw.trim();
      if (!message) continue;
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
    io.emit("status", { username: account.username, status: "stopped" });
    record({ level: code === 0 ? "info" : "error", username: account.username, message: `exited code=${code} signal=${signal}` });
  });

  return { ok: true };
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
    return res.end(JSON.stringify({ ok: true, running: runningList().length }));
  }

  if (req.method === "GET" && (url.pathname === "/" || url.pathname === "/index.html")) {
    res.writeHead(200, { "Content-Type": "text/html; charset=utf-8" });
    return res.end(readFileSync(UI_PATH));
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
          if (!account.password && previous) account.password = previous.password;
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
  socket.emit("hello", { running: runningList(), logs: logs.slice(-200) });

  socket.on("start", (username, ack) => {
    const account = config.accounts.find((a) => a.username.toLowerCase() === String(username).toLowerCase());
    if (!account) return ack?.({ ok: false, error: "unknown account" });
    ack?.(startAccount(account));
  });

  socket.on("start-all", (_, ack) => {
    const results = config.accounts.filter((a) => a.enabled).map((a) => ({ username: a.username, ...startAccount(a) }));
    ack?.({ ok: true, results });
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
  console.log(`ProTanki clicker UI on http://0.0.0.0:${PORT}`);
});
