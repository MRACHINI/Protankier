import dotenv from "dotenv";
dotenv.config();

export const ENABLE_INCOMING = process.env.ENABLE_INCOMING;
export const ENABLE_OUTGOING = process.env.ENABLE_OUTGOING;
export const ENABLE_BASIC_INCOMING = process.env.ENABLE_BASIC_INCOMING;
export const ENABLE_BASIC_OUTGOING = process.env.ENABLE_BASIC_OUTGOING;
export const ENABLE_INFO = process.env.ENABLE_INFO;
export const ENABLE_ERROR = process.env.ENABLE_ERROR;
export const ENABLE_WARNING = process.env.ENABLE_WARNING;
export const IO_EMIT = process.env.IO_EMIT;
export const IO_ONEVENT = process.env.IO_ONEVENT;
export const CAPTCHA_HANDLING = process.env.CAPTCHA_HANDLING == "true";
export const CAPTCHA_THREADS = process.env.CAPTCHA_THREADS;
export const CAPTCHA_POOL_SIZE = process.env.CAPTCHA_POOL_SIZE;
export const CAPSOLVER_API_KEY = process.env.CAPSOLVER_API_KEY;

// ============================================================
// CLICKER CONFIGURATION
// ============================================================

// Clicker timing configuration (in milliseconds)
export const MAX_CLICKER_INTERVAL_TIMEOUT = 525;
export const MIN_CLICKER_INTERVAL_TIMEOUT = 400;

// ============================================================
// CONTINUOUS SPAM MODE
// ============================================================
// When enabled, each account sends join packets continuously
// at staggered intervals to ensure coverage regardless of latency
//
// How it works:
// - Each account sends a packet every 500ms
// - With N accounts, packets arrive every (500ms / N)
// - Example: 50 accounts = packet every 10ms
// - Latency doesn't matter - we're already sending!
//
// Modes:
// - "reactive": Wait for "player left" packet, then respond
// - "continuous": Send packets every 500ms regardless
// ============================================================

export const SPAM_MODE = "continuous";  // "reactive" or "continuous"
export const CONTINUOUS_SPAM_ENABLED = SPAM_MODE === "continuous";

// When in continuous mode, each account sends every 500ms
// This is safe from bans (human-like timing)
export const CONTINUOUS_SPAM_INTERVAL = 500;  // milliseconds

// Burst count when spot opens (for reactive mode)
export const BURST_COUNT = 3;
