// ShadowLend client — wallet detection, faucet claim, on-chain user stats.
// Designed to run from a plain static HTML page (no bundler), pulling deps
// from esm.sh. The IDL is fetched from /idl/shadow_lend.json after `anchor build`.

import {
  Connection,
  PublicKey,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
  Transaction,
} from "https://esm.sh/@solana/web3.js@1.95.8";
import * as anchor from "https://esm.sh/@coral-xyz/anchor@0.30.1?bundle";
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
  getAccount,
} from "https://esm.sh/@solana/spl-token@0.4.14?bundle";

const CONFIG = window.SHADOW_LEND_CONFIG;
const FAUCET_SEED = new TextEncoder().encode("faucet");
const MINT_AUTH_SEED = new TextEncoder().encode("mint-auth");
const CLAIM_SEED = new TextEncoder().encode("claim");
const STATS_SEED = new TextEncoder().encode("stats");

const PROGRAM_ID = new PublicKey(CONFIG.programId);
const connection = new Connection(CONFIG.rpcUrl, "confirmed");

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
  // anchor 0.30+ takes the program id from `idl.address`; the constructor is
  // `new Program(idl, provider)` — passing a separate program id silently
  // makes the PublicKey the "provider" and breaks every later call.
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

// ── PDA helpers ────────────────────────────────────────────
export function pdas(userPk) {
  const [faucet] = PublicKey.findProgramAddressSync([FAUCET_SEED], PROGRAM_ID);
  const [mintAuthority] = PublicKey.findProgramAddressSync(
    [MINT_AUTH_SEED],
    PROGRAM_ID
  );
  const out = { faucet, mintAuthority };
  if (userPk) {
    [out.claim] = PublicKey.findProgramAddressSync(
      [CLAIM_SEED, userPk.toBuffer()],
      PROGRAM_ID
    );
    [out.stats] = PublicKey.findProgramAddressSync(
      [STATS_SEED, userPk.toBuffer()],
      PROGRAM_ID
    );
  }
  return out;
}

// ── On-chain reads (work without a connected wallet) ───────
export async function fetchFaucetConfig() {
  const p = await readClient();
  if (!p) return null;
  try {
    const { faucet } = pdas();
    return await p.account.faucetConfig.fetch(faucet);
  } catch {
    return null;
  }
}

export async function isFaucetReady() {
  return (await fetchFaucetConfig()) !== null;
}

export async function fetchClaimReceipt() {
  const p = await readClient();
  if (!p || !wallet) return null;
  try {
    const { claim } = pdas(wallet.publicKey);
    return await p.account.claimReceipt.fetch(claim);
  } catch {
    return null;
  }
}

export async function fetchUserStats() {
  const p = await readClient();
  if (!p || !wallet) return null;
  try {
    const { stats } = pdas(wallet.publicKey);
    return await p.account.userStats.fetch(stats);
  } catch {
    return null;
  }
}

export async function fetchSlBalance() {
  if (!wallet) return null;
  const faucet = await fetchFaucetConfig();
  if (!faucet) return null;
  try {
    const ata = getAssociatedTokenAddressSync(faucet.mint, wallet.publicKey);
    const acct = await getAccount(connection, ata);
    return Number(acct.amount) / 1e9;
  } catch {
    return 0;
  }
}

// ── Transactions ───────────────────────────────────────────
export async function claimFaucet() {
  if (!program || !wallet) throw new Error("not connected");
  const faucet = await fetchFaucetConfig();
  if (!faucet) throw new Error("faucet not initialized");
  const { faucet: faucetPda, mintAuthority, claim } = pdas(wallet.publicKey);
  const ata = getAssociatedTokenAddressSync(faucet.mint, wallet.publicKey);

  return await program.methods
    .claimFaucet()
    .accounts({
      user: wallet.publicKey,
      faucet: faucetPda,
      mint: faucet.mint,
      mintAuthority,
      recipientAta: ata,
      receipt: claim,
      tokenProgram: TOKEN_PROGRAM_ID,
      associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
      rent: SYSVAR_RENT_PUBKEY,
    })
    .rpc();
}

export async function ensureStatsInited() {
  if (!program || !wallet) return false;
  const existing = await fetchUserStats();
  if (existing) return true;
  const { stats } = pdas(wallet.publicKey);
  await program.methods
    .initUserStats()
    .accounts({
      user: wallet.publicKey,
      stats,
      systemProgram: SystemProgram.programId,
    })
    .rpc();
  return true;
}

export async function recordAction(kind, healthBps = 0) {
  if (!program || !wallet) throw new Error("not connected");
  await ensureStatsInited();
  const { stats } = pdas(wallet.publicKey);
  const variant = { [kind]: {} }; // anchor enum encoding
  return await program.methods
    .recordAction(variant, healthBps)
    .accounts({ user: wallet.publicKey, stats })
    .rpc();
}

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

// Expose for debugging in console.
window.SL = {
  connect,
  disconnect,
  getWallet,
  detectInstalledWallets,
  fetchFaucetConfig,
  isFaucetReady,
  fetchClaimReceipt,
  fetchUserStats,
  fetchSlBalance,
  claimFaucet,
  recordAction,
  ensureStatsInited,
  loadHistory,
  appendHistory,
  PROGRAM_ID,
};
window.dispatchEvent(new Event("sl-client-ready"));
