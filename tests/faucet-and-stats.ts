import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import {
  Keypair,
  PublicKey,
  SystemProgram,
  SYSVAR_RENT_PUBKEY,
  LAMPORTS_PER_SOL,
} from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  ASSOCIATED_TOKEN_PROGRAM_ID,
  getAssociatedTokenAddressSync,
  getAccount,
} from "@solana/spl-token";
import { assert } from "chai";

const FAUCET_SEED = Buffer.from("faucet");
const MINT_AUTH_SEED = Buffer.from("mint-auth");
const CLAIM_SEED = Buffer.from("claim");
const STATS_SEED = Buffer.from("stats");

const CLAIM_AMOUNT = new BN("10000000000000"); // 10_000 * 1e9

describe("ShadowLend — faucet + user stats", () => {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);

  const program = anchor.workspace.ShadowLend as Program<any>;
  const admin = (provider.wallet as anchor.Wallet).payer;

  const mint = Keypair.generate();
  const user = Keypair.generate();

  const [faucetPda] = PublicKey.findProgramAddressSync(
    [FAUCET_SEED],
    program.programId
  );
  const [mintAuthorityPda] = PublicKey.findProgramAddressSync(
    [MINT_AUTH_SEED],
    program.programId
  );

  before(async () => {
    const sig = await provider.connection.requestAirdrop(
      user.publicKey,
      2 * LAMPORTS_PER_SOL
    );
    await provider.connection.confirmTransaction(sig, "confirmed");
  });

  it("initializes the SL faucet with a fresh mint", async () => {
    await program.methods
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

    const faucet = await program.account.faucetConfig.fetch(faucetPda);
    assert.equal(faucet.admin.toBase58(), admin.publicKey.toBase58());
    assert.equal(faucet.mint.toBase58(), mint.publicKey.toBase58());
    assert.ok(faucet.amountPerClaim.eq(CLAIM_AMOUNT));
    assert.equal(faucet.claimCount.toNumber(), 0);
  });

  it("lets a user claim 10,000 SL once", async () => {
    const ata = getAssociatedTokenAddressSync(mint.publicKey, user.publicKey);
    const [receiptPda] = PublicKey.findProgramAddressSync(
      [CLAIM_SEED, user.publicKey.toBuffer()],
      program.programId
    );

    await program.methods
      .claimFaucet()
      .accounts({
        user: user.publicKey,
        faucet: faucetPda,
        mint: mint.publicKey,
        mintAuthority: mintAuthorityPda,
        recipientAta: ata,
        receipt: receiptPda,
        tokenProgram: TOKEN_PROGRAM_ID,
        associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
        rent: SYSVAR_RENT_PUBKEY,
      })
      .signers([user])
      .rpc();

    const tokenAccount = await getAccount(provider.connection, ata);
    assert.equal(tokenAccount.amount.toString(), CLAIM_AMOUNT.toString());

    const receipt = await program.account.claimReceipt.fetch(receiptPda);
    assert.equal(receipt.user.toBase58(), user.publicKey.toBase58());
    assert.ok(receipt.amount.eq(CLAIM_AMOUNT));

    const faucet = await program.account.faucetConfig.fetch(faucetPda);
    assert.equal(faucet.claimCount.toNumber(), 1);
  });

  it("rejects a second claim from the same wallet", async () => {
    const ata = getAssociatedTokenAddressSync(mint.publicKey, user.publicKey);
    const [receiptPda] = PublicKey.findProgramAddressSync(
      [CLAIM_SEED, user.publicKey.toBuffer()],
      program.programId
    );

    let failed = false;
    try {
      await program.methods
        .claimFaucet()
        .accounts({
          user: user.publicKey,
          faucet: faucetPda,
          mint: mint.publicKey,
          mintAuthority: mintAuthorityPda,
          recipientAta: ata,
          receipt: receiptPda,
          tokenProgram: TOKEN_PROGRAM_ID,
          associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID,
          systemProgram: SystemProgram.programId,
          rent: SYSVAR_RENT_PUBKEY,
        })
        .signers([user])
        .rpc();
    } catch (e: any) {
      failed = true;
      // Anchor will surface "already in use" from the System program init.
      assert.match(String(e), /already in use|already exists|0x0/i);
    }
    assert.isTrue(failed, "second claim must fail");
  });

  it("tracks per-wallet stats and bumps counters per action", async () => {
    const [statsPda] = PublicKey.findProgramAddressSync(
      [STATS_SEED, user.publicKey.toBuffer()],
      program.programId
    );

    await program.methods
      .initUserStats()
      .accounts({
        user: user.publicKey,
        stats: statsPda,
        systemProgram: SystemProgram.programId,
      })
      .signers([user])
      .rpc();

    await program.methods
      .recordAction({ supply: {} } as any, 9100)
      .accounts({ user: user.publicKey, stats: statsPda })
      .signers([user])
      .rpc();

    await program.methods
      .recordAction({ borrow: {} } as any, 7600)
      .accounts({ user: user.publicKey, stats: statsPda })
      .signers([user])
      .rpc();

    await program.methods
      .recordAction({ repay: {} } as any, 8400)
      .accounts({ user: user.publicKey, stats: statsPda })
      .signers([user])
      .rpc();

    const stats = await program.account.userStats.fetch(statsPda);
    assert.equal(stats.user.toBase58(), user.publicKey.toBase58());
    assert.equal(stats.proofsSubmitted.toNumber(), 3);
    assert.equal(stats.supplies.toNumber(), 1);
    assert.equal(stats.borrows.toNumber(), 1);
    assert.equal(stats.repays.toNumber(), 1);
    assert.equal(stats.lastHealthBps, 8400);
  });

  after(() => {
    console.log("\n════════════════════════════════════════");
    console.log("  Faucet + UserStats");
    console.log("════════════════════════════════════════");
    console.log("  ✓ Single SL mint, PDA mint authority");
    console.log("  ✓ One claim per wallet (ClaimReceipt PDA)");
    console.log("  ✓ Per-wallet stats PDA, counters per action");
    console.log("════════════════════════════════════════\n");
  });
});
