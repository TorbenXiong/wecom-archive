# SQLite3MultipleCiphers provenance

- Upstream: `https://github.com/utelle/SQLite3MultipleCiphers`
- Release: `v2.5.1`
- SQLite base: `3.53.4`
- Release date: `2026-08-27`
- Asset: `sqlite3mc-2.5.1-sqlite-3.53.4-amalgamation.zip`
- Asset SHA-256: `4125f8ff275ea953dabb3289331b20a0e76d4fc060f57148f4a5df3bf3b0d5e0`
- Vendored source: `sqlite3/sqlite3.c` from `sqlite3mc_amalgamation.c`
- Vendored public headers: `sqlite3/sqlite3.h`, `sqlite3/sqlite3ext.h`
- Upstream `sqlite3/sqlite3.c` SHA-256 before the local bridge:
  `59e30889a7b0106152e6d4fc3c18ac1592f252cb4defbb0a7e618fdecf1a221c`
- Current `sqlite3/sqlite3.c` SHA-256:
  `f41ea1060e10e521352270d422e43c621608e891500937514568b627acdcc51f`
- `sqlite3/sqlite3.h` SHA-256: `919e7f2e8ed1d8f56ac17b412b8971c76aa5d1a879752cc6058f75e7d5910e1d`
- `sqlite3/sqlite3ext.h` SHA-256: `ac9645e5c9ff0cf176efd6e75cb5e98f46295d38e02db5c4d208826a39ab4be`

The surrounding Rust crate is a locally patched copy of `libsqlite3-sys
0.38.2`. Its original MIT license remains in `LICENSE`. The upstream
SQLite3MultipleCiphers MIT license is preserved in `SQLITE3MC_LICENSE`.

The build-script change selects `CODEC_TYPE_AES128` as the default cipher. A
small local C bridges named `sqlite3mc_key_aes128_derived` and
`sqlite3mc_key_aes256_derived` are appended to the amalgamation. They initialize
the upstream wxSQLite3 AES codec and replace its read/write key buffers with an
already-derived 16- or 32-byte key before the first SQL statement. The bridges
do not change page encryption, IV derivation, or cipher implementation behavior.

No source from the separately referenced application is present here.
