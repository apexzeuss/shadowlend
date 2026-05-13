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

const idl = {
  address: PROGRAM_ID,
  metadata: {
    name: "shadow_lend",
    version: "0.1.0",
    spec: "0.1.0",
    description: "Created with Anchor",
  },
  instructions: [
    {
      name: "initialize_faucet",
      discriminator: ixDisc("initialize_faucet"),
      accounts: [
        { name: "admin", writable: true, signer: true },
        {
          name: "faucet",
          writable: true,
          pda: { seeds: [{ kind: "const", value: Array.from(Buffer.from("faucet")) }] },
        },
        { name: "mint", writable: true, signer: true },
        {
          name: "mint_authority",
          pda: { seeds: [{ kind: "const", value: Array.from(Buffer.from("mint-auth")) }] },
        },
        { name: "system_program", address: SystemProgram },
        { name: "token_program", address: TokenProgram },
        { name: "rent", address: "SysvarRent111111111111111111111111111111111" },
      ],
      args: [{ name: "amount_per_claim", type: "u64" }],
    },
    {
      name: "set_claim_amount",
      discriminator: ixDisc("set_claim_amount"),
      accounts: [
        { name: "admin", signer: true },
        {
          name: "faucet",
          writable: true,
          pda: { seeds: [{ kind: "const", value: Array.from(Buffer.from("faucet")) }] },
        },
      ],
      args: [{ name: "amount", type: "u64" }],
    },
    {
      name: "claim_faucet",
      discriminator: ixDisc("claim_faucet"),
      accounts: [
        { name: "user", writable: true, signer: true },
        {
          name: "faucet",
          writable: true,
          pda: { seeds: [{ kind: "const", value: Array.from(Buffer.from("faucet")) }] },
        },
        { name: "mint", writable: true },
        {
          name: "mint_authority",
          pda: { seeds: [{ kind: "const", value: Array.from(Buffer.from("mint-auth")) }] },
        },
        { name: "recipient_ata", writable: true },
        {
          name: "receipt",
          writable: true,
          pda: {
            seeds: [
              { kind: "const", value: Array.from(Buffer.from("claim")) },
              { kind: "account", path: "user" },
            ],
          },
        },
        { name: "token_program", address: TokenProgram },
        { name: "associated_token_program", address: AssociatedTokenProgram },
        { name: "system_program", address: SystemProgram },
        { name: "rent", address: "SysvarRent111111111111111111111111111111111" },
      ],
      args: [],
    },
    {
      name: "init_user_stats",
      discriminator: ixDisc("init_user_stats"),
      accounts: [
        { name: "user", writable: true, signer: true },
        {
          name: "stats",
          writable: true,
          pda: {
            seeds: [
              { kind: "const", value: Array.from(Buffer.from("stats")) },
              { kind: "account", path: "user" },
            ],
          },
        },
        { name: "system_program", address: SystemProgram },
      ],
      args: [],
    },
    {
      name: "record_action",
      discriminator: ixDisc("record_action"),
      accounts: [
        { name: "user", signer: true },
        {
          name: "stats",
          writable: true,
          pda: {
            seeds: [
              { kind: "const", value: Array.from(Buffer.from("stats")) },
              { kind: "account", path: "user" },
            ],
          },
        },
      ],
      args: [
        {
          name: "kind",
          type: { defined: { name: "ActionKind" } },
        },
        { name: "health_bps", type: "u16" },
      ],
    },
  ],
  accounts: [
    { name: "FaucetConfig", discriminator: acctDisc("FaucetConfig") },
    { name: "ClaimReceipt", discriminator: acctDisc("ClaimReceipt") },
    { name: "UserStats", discriminator: acctDisc("UserStats") },
  ],
  events: [
    { name: "FaucetClaimed", discriminator: eventDisc("FaucetClaimed") },
    { name: "ActionRecorded", discriminator: eventDisc("ActionRecorded") },
  ],
  errors: [
    { code: 6000, name: "NotAdmin", msg: "Caller is not the faucet admin." },
    { code: 6001, name: "MintMismatch", msg: "Mint account does not match the faucet config." },
    { code: 6002, name: "Overflow", msg: "Arithmetic overflow." },
    { code: 6003, name: "Unauthorized", msg: "Stats account does not belong to this signer." },
    { code: 6004, name: "Overflow", msg: "Arithmetic overflow." },
  ],
  types: [
    {
      name: "FaucetConfig",
      type: {
        kind: "struct",
        fields: [
          { name: "admin", type: "pubkey" },
          { name: "mint", type: "pubkey" },
          { name: "amount_per_claim", type: "u64" },
          { name: "total_claimed", type: "u64" },
          { name: "claim_count", type: "u64" },
          { name: "bump", type: "u8" },
          { name: "mint_authority_bump", type: "u8" },
        ],
      },
    },
    {
      name: "ClaimReceipt",
      type: {
        kind: "struct",
        fields: [
          { name: "user", type: "pubkey" },
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
      name: "FaucetClaimed",
      type: {
        kind: "struct",
        fields: [
          { name: "user", type: "pubkey" },
          { name: "amount", type: "u64" },
          { name: "claimed_at", type: "i64" },
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
