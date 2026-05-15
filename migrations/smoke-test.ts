// Devnet smoke test of the cross-asset market flow with the admin wallet.
// Run: TS_NODE_TRANSPILE_ONLY=true ANCHOR_PROVIDER_URL=https://api.devnet.solana.com \
//      ANCHOR_WALLET=~/.config/solana/id.json yarn ts-node migrations/smoke-test.ts
import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import { Connection, PublicKey, SystemProgram } from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";
import * as bs58Mod from "bs58";
import * as fs from "fs";

const bs58 = (bs58Mod as any).default || (bs58Mod as any);
const seed = (s: string) => Buffer.from(s);

const cfgSrc = fs.readFileSync(__dirname + "/../app/config.js", "utf8");
const cfgWindow: any = {};
new Function("window", cfgSrc)(cfgWindow);
const cfg = cfgWindow.SHADOW_LEND_CONFIG;

const RECEIVER = new PublicKey(cfg.pythReceiverProgramId);

async function freshestPriceUpdate(
  conn: Connection,
  feedHex: string
): Promise<PublicKey> {
  const accts = await conn.getProgramAccounts(RECEIVER, {
    filters: [
      { dataSize: 134 },
      { memcmp: { offset: 41, bytes: bs58.encode(Buffer.from(feedHex, "hex")) } },
    ],
  });
  if (accts.length === 0) throw new Error(`no PriceUpdateV2 for ${feedHex}`);
  let best = null as null | { pk: PublicKey; pt: number };
  for (const a of accts) {
    const pt = Number(a.account.data.readBigInt64LE(93));
    if (!best || pt > best.pt) best = { pk: a.pubkey, pt };
  }
  return best!.pk;
}

async function main() {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.ShadowLend as Program<any>;
  const user = provider.wallet.publicKey;
  const conn = provider.connection;

  const m = cfg.markets[0]; // SOL
  const mint = new PublicKey(m.mint);
  const mb = mint.toBuffer();
  const pid = program.programId;
  const [market] = PublicKey.findProgramAddressSync([seed("market"), mb], pid);
  const [authority] = PublicKey.findProgramAddressSync([seed("auth"), mb], pid);
  const [vault] = PublicKey.findProgramAddressSync([seed("vault"), mb], pid);
  const [claim] = PublicKey.findProgramAddressSync(
    [seed("claim"), user.toBuffer(), mb],
    pid
  );
  const [position] = PublicKey.findProgramAddressSync(
    [seed("position"), user.toBuffer(), mb],
    pid
  );
  const ata = getAssociatedTokenAddressSync(mint, user);
  const base = (ui: number) => new BN(ui).mul(new BN(10).pow(new BN(9)));

  const priceUpdate = await freshestPriceUpdate(conn, m.feedHex);
  console.log("price_update:", priceUpdate.toBase58());

  // Claim (skip if already done).
  const haveReceipt = await program.account.claimReceipt.fetchNullable(claim);
  if (!haveReceipt) {
    await program.methods
      .claimFaucet()
      .accounts({
        user,
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
    console.log("✓ claim_faucet");
  } else {
    console.log("• already claimed, skipping");
  }

  const supplyAccounts = {
    user,
    market,
    mint,
    position,
    vault,
    authority,
    userAta: ata,
    tokenProgram: TOKEN_PROGRAM_ID,
    associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
    systemProgram: SystemProgram.programId,
  };
  const priceAccounts = { ...supplyAccounts, priceUpdate };

  await program.methods.supply(base(500)).accounts(supplyAccounts).rpc();
  console.log("✓ supply 500");

  // Same-mint borrow must now be rejected: the user has supplied SOL into the
  // SOL market, so borrowing SOL against their own supply is not allowed.
  try {
    await program.methods.borrow(base(100)).accounts(priceAccounts).rpc();
    console.log("✗ same-mint borrow unexpectedly succeeded");
    process.exit(1);
  } catch (e: any) {
    const msg = e?.message || String(e);
    if (/SameMintBorrow/i.test(msg))
      console.log("✓ same-mint borrow correctly rejected (SameMintBorrow)");
    else throw e;
  }

  await program.methods.withdraw(base(500)).accounts(priceAccounts).rpc();
  console.log("✓ withdraw 500");

  const pos = await program.account.position.fetch(position);
  console.log(
    `position → supplied ${pos.supplied.toString()} borrowed ${pos.borrowed.toString()}`
  );
  console.log("\nALL GOOD");
}

main().catch((e) => {
  console.error("SMOKE TEST FAILED:", e.message || e);
  process.exit(1);
});
