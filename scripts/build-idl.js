#!/usr/bin/env node
// Hand-built Anchor 0.30.1 IDL for the shadow-lend program.
// Used as a fallback when `anchor build` can't run the IDL extraction step
// (which requires a Rust nightly toolchain). The discriminators are computed
// from sha256 prefixes the same way Anchor's macros compute them at compile
// time, so the JSON is byte-compatible with what `anchor build` would emit.
//
// Run: node scripts/build-idl.js

const crypto = require("crypto");
const fs = require("fs");
const path = require("path");

const PROGRAM_ID = "5jqXbgExBEnKPahsQineFmMJHNcEvwnniiYvDy81bZCF";

function disc(prefix, name) {
  return Array.from(
    crypto.createHash("sha256").update(`${prefix}:${name}`).digest().subarray(0, 8)
  );
}

const ixDisc = (name) => disc("global", name);
const acctDisc = (name) => disc("account", name);
const eventDisc = (name) => disc("event", name);

const SystemProgram = "11111111111111111111111111111111";
const TokenProgram = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const AssociatedTokenProgram = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const Rent = "SysvarRent111111111111111111111111111111111";

const seedConst = (s) => ({ kind: "const", value: Array.from(Buffer.from(s)) });
const seedAccount = (path) => ({ kind: "account", path });

// Seeds reused across instructions.
const marketPda = (mintPath = "mint") => ({
  seeds: [seedConst("market"), seedAccount(mintPath)],
});
const authPda = { seeds: [seedConst("auth"), seedAccount("mint")] };
const vaultPda = { seeds: [seedConst("vault"), seedAccount("mint")] };
const positionPda = {
  seeds: [seedConst("position"), seedAccount("user"), seedAccount("mint")],
};
const claimPda = {
  seeds: [seedConst("claim"), seedAccount("user"), seedAccount("mint")],
};
const statsPda = { seeds: [seedConst("stats"), seedAccount("user")] };

// supply / withdraw / borrow / repay all share the ModifyPosition context.
const modifyPositionAccounts = [
  { name: "user", writable: true, signer: true },
  { name: "market", writable: true, pda: marketPda() },
  { name: "mint" },
  { name: "position", writable: true, pda: positionPda },
  { name: "vault", writable: true, pda: vaultPda },
  { name: "authority", pda: authPda },
  { name: "user_ata", writable: true },
  { name: "token_program", address: TokenProgram },
  { name: "associated_token_program", address: AssociatedTokenProgram },
  { name: "system_program", address: SystemProgram },
];

const modifyIx = (name) => ({
  name,
  discriminator: ixDisc(name),
  accounts: modifyPositionAccounts,
  args: [{ name: "amount", type: "u64" }],
});

const idl = {
  address: PROGRAM_ID,
  metadata: {
    name: "shadow_lend",
    version: "0.2.0",
    spec: "0.1.0",
    description: "Created with Anchor",
  },
  instructions: [
    {
      name: "initialize_market",
      discriminator: ixDisc("initialize_market"),
      accounts: [
        { name: "admin", writable: true, signer: true },
        { name: "market", writable: true, pda: marketPda() },
        { name: "mint", writable: true, signer: true },
        { name: "authority", pda: authPda },
        { name: "vault", writable: true, pda: vaultPda },
        { name: "system_program", address: SystemProgram },
        { name: "token_program", address: TokenProgram },
      ],
      args: [
        { name: "amount_per_claim", type: "u64" },
        { name: "max_ltv_bps", type: "u16" },
      ],
    },
    {
      name: "set_claim_amount",
      discriminator: ixDisc("set_claim_amount"),
      accounts: [
        { name: "admin", signer: true },
        { name: "market", writable: true, pda: marketPda("market.mint") },
      ],
      args: [{ name: "amount", type: "u64" }],
    },
    {
      name: "claim_faucet",
      discriminator: ixDisc("claim_faucet"),
      accounts: [
        { name: "user", writable: true, signer: true },
        { name: "market", writable: true, pda: marketPda() },
        { name: "mint", writable: true },
        { name: "authority", pda: authPda },
        { name: "recipient_ata", writable: true },
        { name: "receipt", writable: true, pda: claimPda },
        { name: "token_program", address: TokenProgram },
        { name: "associated_token_program", address: AssociatedTokenProgram },
        { name: "system_program", address: SystemProgram },
      ],
      args: [],
    },
    modifyIx("supply"),
    modifyIx("withdraw"),
    modifyIx("borrow"),
    modifyIx("repay"),
    {
      name: "init_user_stats",
      discriminator: ixDisc("init_user_stats"),
      accounts: [
        { name: "user", writable: true, signer: true },
        { name: "stats", writable: true, pda: statsPda },
        { name: "system_program", address: SystemProgram },
      ],
      args: [],
    },
    {
      name: "record_action",
      discriminator: ixDisc("record_action"),
      accounts: [
        { name: "user", signer: true },
        { name: "stats", writable: true, pda: statsPda },
      ],
      args: [
        { name: "kind", type: { defined: { name: "ActionKind" } } },
        { name: "health_bps", type: "u16" },
      ],
    },
  ],
  accounts: [
    { name: "Market", discriminator: acctDisc("Market") },
    { name: "Position", discriminator: acctDisc("Position") },
    { name: "ClaimReceipt", discriminator: acctDisc("ClaimReceipt") },
    { name: "UserStats", discriminator: acctDisc("UserStats") },
  ],
  events: [
    { name: "FaucetClaimed", discriminator: eventDisc("FaucetClaimed") },
    { name: "MarketAction", discriminator: eventDisc("MarketAction") },
    { name: "ActionRecorded", discriminator: eventDisc("ActionRecorded") },
  ],
  errors: [
    { code: 6000, name: "NotAdmin", msg: "Caller is not the market admin." },
    { code: 6001, name: "MintMismatch", msg: "Mint account does not match the market config." },
    { code: 6002, name: "VaultMismatch", msg: "Vault account does not match the market config." },
    { code: 6003, name: "BadLtv", msg: "Max LTV must be between 1 and 9999 bps." },
    { code: 6004, name: "ZeroAmount", msg: "Amount must be greater than zero." },
    { code: 6005, name: "ExceedsLtv", msg: "Borrow would exceed the market's max LTV." },
    {
      code: 6006,
      name: "WouldBeUnhealthy",
      msg: "Withdrawing this much would leave the position undercollateralized.",
    },
    { code: 6007, name: "InsufficientCollateral", msg: "Not enough supplied collateral." },
    {
      code: 6008,
      name: "InsufficientLiquidity",
      msg: "Vault does not have enough liquidity for this borrow.",
    },
    { code: 6009, name: "NothingToRepay", msg: "Position has no outstanding debt to repay." },
    { code: 6010, name: "Overflow", msg: "Arithmetic overflow." },
    { code: 6011, name: "Unauthorized", msg: "Stats account does not belong to this signer." },
    { code: 6012, name: "Overflow", msg: "Arithmetic overflow." },
  ],
  types: [
    {
      name: "Market",
      type: {
        kind: "struct",
        fields: [
          { name: "admin", type: "pubkey" },
          { name: "mint", type: "pubkey" },
          { name: "vault", type: "pubkey" },
          { name: "amount_per_claim", type: "u64" },
          { name: "max_ltv_bps", type: "u16" },
          { name: "total_supplied", type: "u64" },
          { name: "total_borrowed", type: "u64" },
          { name: "total_claimed", type: "u64" },
          { name: "claim_count", type: "u64" },
          { name: "bump", type: "u8" },
          { name: "authority_bump", type: "u8" },
        ],
      },
    },
    {
      name: "Position",
      type: {
        kind: "struct",
        fields: [
          { name: "user", type: "pubkey" },
          { name: "market", type: "pubkey" },
          { name: "supplied", type: "u64" },
          { name: "borrowed", type: "u64" },
          { name: "bump", type: "u8" },
        ],
      },
    },
    {
      name: "ClaimReceipt",
      type: {
        kind: "struct",
        fields: [
          { name: "user", type: "pubkey" },
          { name: "mint", type: "pubkey" },
          { name: "amount", type: "u64" },
          { name: "claimed_at", type: "i64" },
          { name: "bump", type: "u8" },
        ],
      },
    },
    {
      name: "UserStats",
      type: {
        kind: "struct",
        fields: [
          { name: "user", type: "pubkey" },
          { name: "created_at", type: "i64" },
          { name: "last_action_at", type: "i64" },
          { name: "proofs_submitted", type: "u64" },
          { name: "supplies", type: "u64" },
          { name: "borrows", type: "u64" },
          { name: "repays", type: "u64" },
          { name: "liquidations", type: "u64" },
          { name: "last_health_bps", type: "u16" },
          { name: "bump", type: "u8" },
        ],
      },
    },
    {
      name: "ActionKind",
      type: {
        kind: "enum",
        variants: [
          { name: "Supply" },
          { name: "Borrow" },
          { name: "Repay" },
          { name: "Liquidation" },
        ],
      },
    },
    {
      name: "MarketActionKind",
      type: {
        kind: "enum",
        variants: [
          { name: "Supply" },
          { name: "Withdraw" },
          { name: "Borrow" },
          { name: "Repay" },
        ],
      },
    },
    {
      name: "FaucetClaimed",
      type: {
        kind: "struct",
        fields: [
          { name: "user", type: "pubkey" },
          { name: "mint", type: "pubkey" },
          { name: "amount", type: "u64" },
          { name: "claimed_at", type: "i64" },
        ],
      },
    },
    {
      name: "MarketAction",
      type: {
        kind: "struct",
        fields: [
          { name: "user", type: "pubkey" },
          { name: "mint", type: "pubkey" },
          { name: "kind", type: { defined: { name: "MarketActionKind" } } },
          { name: "supplied", type: "u64" },
          { name: "borrowed", type: "u64" },
        ],
      },
    },
    {
      name: "ActionRecorded",
      type: {
        kind: "struct",
        fields: [
          { name: "user", type: "pubkey" },
          { name: "kind", type: { defined: { name: "ActionKind" } } },
          { name: "health_bps", type: "u16" },
          { name: "at", type: "i64" },
        ],
      },
    },
  ],
};

const outDir = path.join(__dirname, "..", "target", "idl");
fs.mkdirSync(outDir, { recursive: true });
const outPath = path.join(outDir, "shadow_lend.json");
fs.writeFileSync(outPath, JSON.stringify(idl, null, 2));
console.log("wrote", outPath);

const appIdl = path.join(__dirname, "..", "app", "idl", "shadow_lend.json");
fs.mkdirSync(path.dirname(appIdl), { recursive: true });
fs.writeFileSync(appIdl, JSON.stringify(idl, null, 2));
console.log("wrote", appIdl);
