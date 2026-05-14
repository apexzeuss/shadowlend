// ShadowLend client — wallet detection + on-chain multi-asset markets.
// Designed to run from a plain static HTML page (no bundler), pulling deps
// from esm.sh. The IDL is fetched from /idl/shadow_lend.json.

import {
  Connection,
  PublicKey,
  SystemProgram,
} from "https://esm.sh/@solana/web3.js@1.95.8";
import * as anchor from "https://esm.sh/@coral-xyz/anchor@0.30.1?bundle";
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
  getAccount,
} from "https://esm.sh/@solana/spl-token@0.4.14?bundle";

const CONFIG = window.SHADOW_LEND_CONFIG;
const MARKET_SEED = new TextEncoder().encode("market");
const AUTH_SEED = new TextEncoder().encode("auth");
const VAULT_SEED = new TextEncoder().encode("vault");
const CLAIM_SEED = new TextEncoder().encode("claim");
const POSITION_SEED = new TextEncoder().encode("position");
const STATS_SEED = new TextEncoder().encode("stats");

const PROGRAM_ID = new PublicKey(CONFIG.programId);
const connection = new Connection(CONFIG.rpcUrl, "confirmed");
const MARKETS = CONFIG.markets || [];

let idl = null;
let program = null; // signing program (built once a wallet connects)
let readProgram = null; // wallet-less program for read-only fetches
let wallet = null; // { name, publicKey, adapter }

// ── IDL loader ─────────────────────────────────────────────
async function loadIdl() {
  if (idl) return idl;
  try {
    const res = await fetch("./idl/shadow_lend.json");
    if (!res.ok) throw new Error("missing IDL");
    idl = await res.json();
    return idl;
  } catch {
    return null;
  }
}

// A provider with a dummy wallet — usable for `program.account.X.fetch(...)`
// but throws if anyone tries to sign with it.
function readonlyProvider() {
  return new anchor.AnchorProvider(
    connection,
    {
      publicKey: PublicKey.default,
      signTransaction: () => {
        throw new Error("read-only provider cannot sign");
      },
      signAllTransactions: () => {
        throw new Error("read-only provider cannot sign");
      },
    },
    { commitment: "confirmed" }
  );
}

async function ensureReadProgram() {
  if (readProgram) return readProgram;
  await loadIdl();
  if (!idl) return null;
  readProgram = new anchor.Program(idl, readonlyProvider());
  return readProgram;
}

function buildProgram() {
  if (!idl || !wallet) return null;
  const provider = new anchor.AnchorProvider(
    connection,
    {
      publicKey: wallet.publicKey,
      signTransaction: (tx) => wallet.adapter.signTransaction(tx),
      signAllTransactions: (txs) => wallet.adapter.signAllTransactions(txs),
    },
    { commitment: "confirmed" }
  );
  return new anchor.Program(idl, provider);
}

// Returns whichever program is appropriate for reads (prefers the signing
// program if a wallet is connected, falls back to the read-only one).
async function readClient() {
  if (program) return program;
  return await ensureReadProgram();
}

// ── Wallet detection ───────────────────────────────────────
export function detectInstalledWallets() {
  const list = [];
  if (window.solflare?.isSolflare) list.push("solflare");
  // Phantom injects at window.phantom.solana, and also (older builds /
  // some setups) at the legacy window.solana.
  if (window.phantom?.solana?.isPhantom || window.solana?.isPhantom)
    list.push("phantom");
  if (window.backpack?.isBackpack) list.push("backpack");
  return list;
}

function adapterFor(name) {
  if (name === "solflare") return window.solflare;
  if (name === "phantom")
    return window.phantom?.solana ||
      (window.solana?.isPhantom ? window.solana : null);
  if (name === "backpack") return window.backpack;
  return null;
}

// Guards against a second connect() landing while the wallet popup from the
// first is still open — that overlap is itself a common source of the opaque
// "Unexpected error" (-32603).
let connecting = null;

export async function connect(name) {
  if (connecting) return connecting;
  connecting = doConnect(name).finally(() => {
    connecting = null;
  });
  return connecting;
}

async function doConnect(name) {
  const adapter = adapterFor(name);
  if (!adapter) throw new Error(`${name} is not installed`);

  let resp;
  // If the wallet is already authorized for this site, reuse that session —
  // calling connect() again on an already-connected adapter is another way
  // wallets throw "Unexpected error".
  if (adapter.isConnected && adapter.publicKey) {
    resp = { publicKey: adapter.publicKey };
  } else {
    try {
      resp = await adapter.connect();
    } catch (e) {
      // Phantom/Solflare collapse a range of wallet-side problems into an
      // opaque `{ code: -32603, message: "Unexpected error" }` — a sleeping
      // extension service worker, stale connection state, or an imported
      // account that can't sign. Retry once (covers the transient wake-up
      // case), then fail with something the user can actually act on.
      const opaque =
        e?.code === -32603 || /unexpected error/i.test(e?.message || "");
      if (!opaque) throw e;
      try {
        resp = await adapter.connect();
      } catch (e2) {
        throw new Error(
          `${name} refused the connection (code ${e2?.code ?? "?"}). ` +
            "Update the wallet extension, switch to a non-imported account, " +
            "or reload the page and retry."
        );
      }
    }
  }

  const pk = resp?.publicKey || adapter.publicKey;
  if (!pk) throw new Error(`${name} connected but returned no public key`);
  wallet = {
    name,
    publicKey: new PublicKey(pk.toString()),
    adapter,
  };
  await loadIdl();
  program = buildProgram();
  return wallet;
}

export async function disconnect() {
  if (wallet?.adapter?.disconnect) {
    try {
      await wallet.adapter.disconnect();
    } catch {}
  }
  wallet = null;
  program = null;
}

export function getWallet() {
  return wallet;
}

// ── Markets + PDA helpers ──────────────────────────────────
export function getMarkets() {
  return MARKETS;
}

export function getMarketConfig(marketId) {
  return MARKETS.find((m) => m.id === marketId) || null;
}

// All program addresses tied to a given market (and optionally a user).
export function marketPdas(marketId, userPk) {
  const cfg = getMarketConfig(marketId);
  if (!cfg) throw new Error(`unknown market: ${marketId}`);
  const mint = new PublicKey(cfg.mint);
  const mintBuf = mint.toBuffer();
  const [market] = PublicKey.findProgramAddressSync(
    [MARKET_SEED, mintBuf],
    PROGRAM_ID
  );
  const [authority] = PublicKey.findProgramAddressSync(
    [AUTH_SEED, mintBuf],
    PROGRAM_ID
  );
  const [vault] = PublicKey.findProgramAddressSync(
    [VAULT_SEED, mintBuf],
    PROGRAM_ID
  );
  const out = { cfg, mint, market, authority, vault };
  if (userPk) {
    [out.claim] = PublicKey.findProgramAddressSync(
      [CLAIM_SEED, userPk.toBuffer(), mintBuf],
      PROGRAM_ID
    );
    [out.position] = PublicKey.findProgramAddressSync(
      [POSITION_SEED, userPk.toBuffer(), mintBuf],
      PROGRAM_ID
    );
    out.ata = getAssociatedTokenAddressSync(mint, userPk);
  }
  return out;
}

function statsPda(userPk) {
  const [stats] = PublicKey.findProgramAddressSync(
    [STATS_SEED, userPk.toBuffer()],
    PROGRAM_ID
  );
  return stats;
}

const pow10 = (n) => new anchor.BN(10).pow(new anchor.BN(n));
// UI amount (whole tokens, may be fractional) → base-unit BN.
function toBase(uiAmount, decimals) {
  const [whole, frac = ""] = String(uiAmount).split(".");
  const fracPadded = (frac + "0".repeat(decimals)).slice(0, decimals);
  return new anchor.BN(whole || "0")
    .mul(pow10(decimals))
    .add(new anchor.BN(fracPadded || "0"));
}
const fromBase = (bn, decimals) =>
  Number(bn.toString()) / Math.pow(10, decimals);

// ── On-chain reads (work without a connected wallet) ───────
export async function fetchMarket(marketId) {
  const p = await readClient();
  if (!p) return null;
  try {
    const { market } = marketPdas(marketId);
    return await p.account.market.fetch(market);
  } catch {
    return null;
  }
}

// Every market, with its on-chain config merged onto the static config.
export async function fetchAllMarkets() {
  return Promise.all(
    MARKETS.map(async (m) => ({ ...m, onchain: await fetchMarket(m.id) }))
  );
}

// True once at least one market exists on-chain.
export async function isFaucetReady() {
  for (const m of MARKETS) {
    if (await fetchMarket(m.id)) return true;
  }
  return false;
}

export async function fetchClaimReceipt(marketId) {
  const p = await readClient();
  if (!p || !wallet) return null;
  try {
    const { claim } = marketPdas(marketId, wallet.publicKey);
    return await p.account.claimReceipt.fetch(claim);
  } catch {
    return null;
  }
}

export async function fetchPosition(marketId) {
  const p = await readClient();
  if (!p || !wallet) return null;
  try {
    const { position } = marketPdas(marketId, wallet.publicKey);
    const pos = await p.account.position.fetch(position);
    const dec = getMarketConfig(marketId).decimals;
    return {
      supplied: fromBase(pos.supplied, dec),
      borrowed: fromBase(pos.borrowed, dec),
      suppliedRaw: pos.supplied,
      borrowedRaw: pos.borrowed,
    };
  } catch {
    return null;
  }
}

export async function fetchTokenBalance(marketId) {
  if (!wallet) return 0;
  try {
    const { mint } = marketPdas(marketId);
    const ata = getAssociatedTokenAddressSync(mint, wallet.publicKey);
    const acct = await getAccount(connection, ata);
    const dec = getMarketConfig(marketId).decimals;
    return Number(acct.amount) / Math.pow(10, dec);
  } catch {
    return 0;
  }
}

// One round-trip the UI can lean on: per-market state for the connected wallet.
export async function fetchUserMarketState() {
  if (!wallet) return [];
  return Promise.all(
    MARKETS.map(async (m) => {
      const [onchain, receipt, position, balance] = await Promise.all([
        fetchMarket(m.id),
        fetchClaimReceipt(m.id),
        fetchPosition(m.id),
        fetchTokenBalance(m.id),
      ]);
      const supplied = position?.supplied || 0;
      const borrowed = position?.borrowed || 0;
      // Health factor: collateral value at max LTV divided by debt. >1 is safe.
      const maxLtv = m.maxLtvBps / 10_000;
      const health =
        borrowed > 0 ? (supplied * maxLtv) / borrowed : null;
      return {
        ...m,
        onchain,
        claimed: !!receipt,
        balance,
        supplied,
        borrowed,
        health,
        borrowable: Math.max(0, supplied * maxLtv - borrowed),
      };
    })
  );
}

// ── User stats (the private-action / proof demo) ───────────
export async function fetchUserStats() {
  const p = await readClient();
  if (!p || !wallet) return null;
  try {
    return await p.account.userStats.fetch(statsPda(wallet.publicKey));
  } catch {
    return null;
  }
}

export async function ensureStatsInited() {
  if (!program || !wallet) return false;
  if (await fetchUserStats()) return true;
  await program.methods
    .initUserStats()
    .accounts({
      user: wallet.publicKey,
      stats: statsPda(wallet.publicKey),
      systemProgram: SystemProgram.programId,
    })
    .rpc();
  return true;
}

export async function recordAction(kind, healthBps = 0) {
  if (!program || !wallet) throw new Error("not connected");
  await ensureStatsInited();
  const variant = { [kind]: {} }; // anchor enum encoding
  return await program.methods
    .recordAction(variant, healthBps)
    .accounts({ user: wallet.publicKey, stats: statsPda(wallet.publicKey) })
    .rpc();
}

// ── Transactions ───────────────────────────────────────────
export async function claimFaucet(marketId) {
  if (!program || !wallet) throw new Error("not connected");
  const { cfg, mint, market, authority, claim, ata } = marketPdas(
    marketId,
    wallet.publicKey
  );
  if (!(await fetchMarket(marketId)))
    throw new Error(`${cfg.label} market not initialized`);

  return await program.methods
    .claimFaucet()
    .accounts({
      user: wallet.publicKey,
      market,
      mint,
      authority,
      recipientAta: ata,
      receipt: claim,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .rpc();
}

// supply / withdraw / borrow / repay all hit the same account context.
async function modifyPosition(method, marketId, uiAmount) {
  if (!program || !wallet) throw new Error("not connected");
  const { cfg, mint, market, authority, vault, position, ata } = marketPdas(
    marketId,
    wallet.publicKey
  );
  const amount = toBase(uiAmount, cfg.decimals);
  if (amount.lten(0)) throw new Error("amount must be greater than zero");

  return await program.methods[method](amount)
    .accounts({
      user: wallet.publicKey,
      market,
      mint,
      position,
      vault,
      authority,
      userAta: ata,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .rpc();
}

export const supply = (marketId, uiAmount) =>
  modifyPosition("supply", marketId, uiAmount);
export const withdraw = (marketId, uiAmount) =>
  modifyPosition("withdraw", marketId, uiAmount);
export const borrow = (marketId, uiAmount) =>
  modifyPosition("borrow", marketId, uiAmount);
export const repay = (marketId, uiAmount) =>
  modifyPosition("repay", marketId, uiAmount);

// ── Local history (IndexedDB) ──────────────────────────────
const DB_NAME = "shadowlend";
const STORE = "history";

function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, 1);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(STORE)) {
        const s = db.createObjectStore(STORE, {
          keyPath: "id",
          autoIncrement: true,
        });
        s.createIndex("by_wallet", "wallet");
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

export async function appendHistory(entry) {
  if (!wallet) return;
  const db = await openDb();
  await new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, "readwrite");
    tx.objectStore(STORE).add({
      wallet: wallet.publicKey.toBase58(),
      at: Date.now(),
      ...entry,
    });
    tx.oncomplete = resolve;
    tx.onerror = () => reject(tx.error);
  });
  db.close();
}

export async function loadHistory(limit = 25) {
  if (!wallet) return [];
  const db = await openDb();
  const result = await new Promise((resolve, reject) => {
    const tx = db.transaction(STORE, "readonly");
    const store = tx.objectStore(STORE);
    const idx = store.index("by_wallet");
    const req = idx.getAll(wallet.publicKey.toBase58());
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
  db.close();
  return result.sort((a, b) => b.at - a.at).slice(0, limit);
}

// Expose for inline scripts + console debugging.
window.SL = {
  connect,
  disconnect,
  getWallet,
  detectInstalledWallets,
  getMarkets,
  getMarketConfig,
  marketPdas,
  fetchMarket,
  fetchAllMarkets,
  isFaucetReady,
  fetchClaimReceipt,
  fetchPosition,
  fetchTokenBalance,
  fetchUserMarketState,
  fetchUserStats,
  ensureStatsInited,
  recordAction,
  claimFaucet,
  supply,
  withdraw,
  borrow,
  repay,
  loadHistory,
  appendHistory,
  PROGRAM_ID,
};
window.dispatchEvent(new Event("sl-client-ready"));
