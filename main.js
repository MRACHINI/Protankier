import { Server } from "socket.io";
import { createServer } from "http";
import { readFileSync } from "fs";
import { fileURLToPath } from "url";
import { dirname, join } from "path";
import clickAccount, { submitLoginCaptcha } from "./Applications/clickAccount.js";
import leaveAccount from "./Applications/leaveAccount.js";
import { refreshServerList } from "./Connections/control/controlChannel.js";
import { SPAM_MODE } from "./config.js";

// Keep the server alive if a single account handler throws
process.on("uncaughtException", (error) => {
  console.error("Uncaught exception:", error);
});
process.on("unhandledRejection", (reason) => {
  console.error("Unhandled rejection:", reason);
});

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

let client;
const maxConcurrency = 50;
let activeRequests = 0;
const queue = [];

// Read the HTML file
const htmlPath = join(__dirname, "public", "index.html");
const htmlContent = readFileSync(htmlPath, "utf-8");

// Load current game server list (falls back to hardcoded ports if unavailable)
await refreshServerList();

// Create HTTP server
const httpServer = createServer((req, res) => {
  if (req.url === "/" || req.url === "/index.html") {
    res.writeHead(200, { "Content-Type": "text/html" });
    res.end(htmlContent);
  } else if (req.url === "/health") {
    res.writeHead(200, { "Content-Type": "application/json" });
    res.end(JSON.stringify({ status: "ok", mode: SPAM_MODE }));
  } else {
    res.writeHead(404);
    res.end("Not found");
  }
});

const PORT = process.env.PORT || 3006;
httpServer.listen(PORT, "0.0.0.0", () => {
  console.log(`🚀 Listening on 0.0.0.0:${PORT}`);
});

const io = new Server(httpServer, {
  cors: {
    origin: "*",
    methods: ["GET", "POST"],
  },
});

console.log("🎮 Tanki Clicker Server");
console.log(`📊 Mode: ${SPAM_MODE}`);
console.log(`🌐 UI: http://localhost:3006`);

io.on("connection", async (socket) => {
  client = socket;
  console.log("New server connection");

  socket.onAny((event, ...args) => {
    if (event === "click" && args[0]) {
      const { password, ...safeData } = args[0];
      console.log(event, [{ ...safeData, password: "***" }]);
      return;
    }
    if (event === "submit-login-captcha" && args[0]) {
      const { token, ...safeData } = args[0];
      console.log(event, [{ ...safeData, token: "***" }]);
      return;
    }
    console.log(event, args);
  });

  socket.on("click", async (data) => {
    let { account, password, rank, battleLink, side, tankiConfig, proxyUrl } = data;
    side = side == "Alpha" ? "A" : side == "Bravo" ? "B" : "J";
    queue.push({ account, password, rank, battleLink, side, socket, tankiConfig: JSON.parse(tankiConfig), proxyUrl });
    socket.emit("update-status", { account: account, status: "Queue" });
    processQueue();
  });

  socket.on("unglitch-account", async (data) => {
    const { account, password, tankiConfig, proxyUrl } = data;
    socket.emit("update-status", { account: account, status: "Unglitching" });
    leaveAccount(account, password, true, null, socket, JSON.parse(tankiConfig), proxyUrl);
  });

  socket.on("restart-clicker", async (data) => {
    // Restarts pm2
    socket.emit("new-message", { color: "green", message: `Clicker ${data} was restarted.` });
    process.exit();
  });

  socket.on("submit-login-captcha", (data) => {
    const { account, token } = data || {};
    const accepted = submitLoginCaptcha(account, token);
    socket.emit("login-captcha-result", {
      account,
      ok: accepted,
      message: accepted
        ? "Token submitted, waiting for the server..."
        : "No pending captcha for this account (it may have expired).",
    });
  });
});

async function processQueue() {
  if (activeRequests >= maxConcurrency || queue.length === 0) {
    return;
  }

  const batchCount = Math.min(maxConcurrency - activeRequests, queue.length);
  const batch = queue.splice(0, batchCount);
  activeRequests += batchCount;

  await (async () => {
    await Promise.all(
      batch.map(async ({ account, password, rank, battleLink, side, socket, tankiConfig, proxyUrl }) => {
        try {
          console.log(`Processing account: ${account}`);
          await clickAccount(account, password, rank, battleLink, side, socket, tankiConfig, proxyUrl);
          console.log(`Successfully processed account: ${account}`);
        } catch (error) {
          console.log(`Error processing account: ${account}`);
          console.error(error);
        } finally {
          activeRequests--;
        }
      })
    );
  })();

  processQueue(); // Call processQueue after the batch has been processed
}

function updateStatus(account, status) {
  client.emit("update-status", { account: account, status: status });
}

export default updateStatus;
