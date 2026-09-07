/** Keeps every test off the real login keychain, which providers otherwise fall back to. */
process.env.KYORA_SWITCH_NO_KEYCHAIN = "1"
