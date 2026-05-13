// ShadowLend frontend config.
// Update `programId` once the program is deployed, and `faucetMint` after
// initializeFaucet is run on the target cluster.
window.SHADOW_LEND_CONFIG = {
  cluster: "devnet",
  rpcUrl: "https://api.devnet.solana.com",
  programId: "5jqXbgExBEnKPahsQineFmMJHNcEvwnniiYvDy81bZCF",
  // Populated by the deploy script after `initialize_faucet`.
  faucetMint: null,
  claimAmountUi: 10_000,
};
