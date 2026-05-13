// Initializes the SL faucet on the configured cluster.
//
//   1. Make sure the program is deployed:  anchor deploy --provider.cluster devnet
//   2. Run this script:                    yarn ts-node migrations/initialize-faucet.ts
//   3. Copy the printed `faucetMint` value into app/config.js
//
// It is safe to re-run: if the faucet config PDA already exists, the script
// will skip initialization and just print the existing config.

import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
} from "@solana/web3.js";
import { TOKEN_PROGRAM_ID } from "@solana/spl-token";
import * as fs from "fs";
import * as path from "path";

const CLAIM_AMOUNT = new BN("10000000000000"); // 10_000 SL · 9 decimals

async function main() {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.ShadowLend as Program<any>;
  const admin = (provider.wallet as anchor.Wallet).payer;

  const [faucetPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("faucet")],
    program.programId
  );
  const [mintAuthorityPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("mint-auth")],
    program.programId
  );

  const existing = await program.account.faucetConfig.fetchNullable(faucetPda);
  if (existing) {
    console.log("Faucet already initialized.");
    console.log("  faucet PDA:    ", faucetPda.toBase58());
    console.log("  mint:          ", existing.mint.toBase58());
    console.log("  amount/claim:  ", existing.amountPerClaim.toString());
    console.log("  claim count:   ", existing.claimCount.toString());
    writeConfig(existing.mint);
    return;
  }

  const mint = Keypair.generate();
  console.log("Initializing faucet:");
  console.log("  program:       ", program.programId.toBase58());
  console.log("  admin:         ", admin.publicKey.toBase58());
  console.log("  faucet PDA:    ", faucetPda.toBase58());
  console.log("  mint authority:", mintAuthorityPda.toBase58());
  console.log("  new SL mint:   ", mint.publicKey.toBase58());

  const sig = await program.methods
    .initializeFaucet(CLAIM_AMOUNT)
    .accounts({
      admin: admin.publicKey,
      faucet: faucetPda,
      mint: mint.publicKey,
      mintAuthority: mintAuthorityPda,
      systemProgram: SystemProgram.programId,
      tokenProgram: TOKEN_PROGRAM_ID,
      rent: SYSVAR_RENT_PUBKEY,
    })
    .signers([mint])
    .rpc();

  console.log("  ✓ tx:", sig);
  writeConfig(mint.publicKey);
}

function writeConfig(mint: PublicKey) {
  const configPath = path.join(__dirname, "..", "app", "config.js");
  if (!fs.existsSync(configPath)) {
    console.log("  (app/config.js not found — skipping config write)");
    return;
  }
  const original = fs.readFileSync(configPath, "utf8");
  const updated = original.replace(
    /faucetMint:\s*[^,]+,/,
    `faucetMint: "${mint.toBase58()}",`
  );
  if (updated !== original) {
    fs.writeFileSync(configPath, updated);
    console.log("  ✓ wrote faucetMint to app/config.js");
  } else {
    console.log(
      "  (could not auto-update app/config.js — set faucetMint manually)"
    );
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
