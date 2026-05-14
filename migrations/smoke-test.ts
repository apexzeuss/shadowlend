// Quick devnet smoke test of the market flow with the admin wallet.
// Run: TS_NODE_TRANSPILE_ONLY=true ANCHOR_PROVIDER_URL=https://api.devnet.solana.com \
//      ANCHOR_WALLET=~/.config/solana/id.json yarn ts-node migrations/smoke-test.ts
import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import { PublicKey, SystemProgram } from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
} from "@solana/spl-token";
import * as fs from "fs";

const seed = (s: string) => Buffer.from(s);
const cfgSrc = fs.readFileSync(__dirname + "/../app/config.js", "utf8");
const cfgWindow: any = {};
new Function("window", cfgSrc)(cfgWindow);
const cfg = cfgWindow.SHADOW_LEND_CONFIG;

async function main() {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.ShadowLend as Program<any>;
  const user = provider.wallet.publicKey;

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

  const modifyAccounts = {
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

  // claim (skip if already claimed)
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
    console.log("• already claimed, skipping claim");
  }

  await program.methods.supply(base(100)).accounts(modifyAccounts).rpc();
  console.log("✓ supply 100");

  await program.methods.borrow(base(50)).accounts(modifyAccounts).rpc();
  console.log("✓ borrow 50");

  await program.methods.repay(base(50)).accounts(modifyAccounts).rpc();
  console.log("✓ repay 50");

  await program.methods.withdraw(base(100)).accounts(modifyAccounts).rpc();
  console.log("✓ withdraw 100");

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
