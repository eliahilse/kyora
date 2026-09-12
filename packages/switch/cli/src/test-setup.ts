/**
 * Belt for the NODE_ENV guard in `keychainSupported`. Preloaded from both the root
 * and the app bunfig, so no working directory can miss it the way the app-only one did.
 */
process.env.KYORA_SWITCH_NO_KEYCHAIN = "1"
