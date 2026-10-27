# Ledger status

_Rewritten by the leader on every ledger commit._

done 0/174 · in-flight — · blocked 8 (human: M0-001, M0-012, M0-014, M1B-001, M1B-010, M3-016, M6-002, M6-006) · attempts 0/250

## Next up

1. **M0-002** (leader): Cargo workspace skeleton with module stubs + Rust CI.
2. **First worker batch**, all with disjoint paths and no human input:
   - M0-003 dev toolchain script
   - M0-006 ACP types
   - M0-007 AG-UI types
   - M0-008a Sprites client codegen

   Leader items to run alongside: M0-004 (A2A type source decision) and M1B-003 (client gates).

3. **Remote Control track (M2b)** needs no Sprites token, so it can run as soon as the hub (M2) and client-core exist. It is the fastest route to a usable start-on-laptop, continue-on-phone product.
4. **Then**: M1A-001, M5-001, M0-005, M1A-002, M1B-002, M2-001.

## Waiting on the human

| Item    | Needed                                                                                       |
| ------- | -------------------------------------------------------------------------------------------- |
| M0-001  | Restricted Sprites org token as the `SPRITES_API_TOKEN` env secret, then restart the session |
| M0-012  | Model-provider connector in the Sprites org                                                  |
| M0-014  | `TS_AUTHKEY` + `WG_TEST_PEER` env secrets                                                    |
| M1B-001 | Apple Developer account, iPhone, Android phone, Mac / macOS runners                          |
| M6-002  | Fly app-scoped deploy token for production promotion                                         |
