# Interop check of the BR Matter client (to be run once after flashing; nothing here was run)

The client (`firmware/br/main/matter_client.c`, `matter_proto.c`) is verified on the host against the Matter SDK test vectors and an independent Python CASE responder (`firmware/br/test/run.sh`). What no host test can prove is that a real Matter stack accepts it. These steps do that, cheapest first. Do not run them against the production thermostats before step 2 passed.

Prerequisites: BR flashed with this build (`GET /watchdog/status` shows `armed_capable: true` once credentials are stored), `secrets/br_watchdog_creds.json` from `provision.sh STEP=cert`, ACL entry from `STEP=acl`.

## 0. Credential sanity (no radio)

- `jq -r '.ipk' secrets/br_watchdog_creds.json` must be `74656d706f726172792069706b203031` ("temporary ipk 01"). That is `GroupTesting::DefaultIpkValue` in connectedhomeip, which `ExampleOperationalCredentialsIssuer` sends in AddNOC for every chip-tool commissioned node. A different IPK shows up as StatusReport NO_SHARED_TRUST_ROOTS (BR error string `case_nosharedroot`), everything else being correct.
- The BR derives the operational IPK itself (HKDF over the epoch key with the compressed fabric id); this derivation is tested against the spec vector.

## 1. Own test device with a known-good peer (chip-tool all-clusters-app)

On the PC or Minix (not on the BR), in a throwaway directory so the production `chip-storage` is untouched:

1. `docker run --rm --network host <image with all-clusters-app>` (or the Linux build `chip-all-clusters-app --discriminator 3840 --KVS /tmp/ac-kvs`), IPv6 UDP port 5540.
2. `chip-tool --storage-directory /tmp/ct pairing onnetwork 77 20202021` (fabric is created by chip-tool, temporary IPK as above).
3. Generate BR credentials against that storage: `STORAGE=/tmp/ct STEP=cert ./provision.sh` (needs `CHIP_CERT_BIN`/the chipcert image), ACL for node 77: `chip-tool accesscontrol write acl '[...existing..., {"fabricIndex":1,"privilege":3,"authMode":2,"subjects":[<controller node id>],"targets":[{"cluster":513,"endpoint":1,"deviceType":null}]}]' 77 0`.
4. The BR cannot reach `localhost`. Run the client on a Linux host instead: the same C files compile there with the mbedTLS from the IDF tree (see `firmware/br/test/run.sh`); `firmware/br/test/test_loopback.c` is the template for the small driver, but it currently takes its credentials from the test vectors and talks to 127.0.0.1, so the driver has to be adapted to read `br_watchdog_creds.json` (`wd_parse_credentials` from `watchdog_core.c` does exactly that) and the all-clusters-app address. Expected: rc 0, then `chip-tool thermostat read occupied-heating-setpoint 77 1` returns the written value (all-clusters-app exposes a Thermostat on endpoint 1; if the attribute is absent the client reports `write_status_86` UNSUPPORTED_ATTRIBUTE, which still proves CASE + IM).
5. Wrong node id, wrong IPK, expired/foreign RCAC must each give the named error (`case_nosharedroot`, `cert_*`), never a write.

## 2. Real thermostat, one node, attended

1. `curl -H "X-Heatd-Token: $T" -d '{"timeout_s":60,"setpoint_c":18.0,"nodes":[8],"endpoint":1}' http://192.168.178.33/watchdog/heartbeat` (one node only; the heartbeat call also needs a short timeout, restore afterwards).
2. `curl -H ... -d '{"mode":"armed"}' http://192.168.178.33/watchdog/mode`; stop sending heartbeats; after 60 s the trigger fires.
3. Watch `GET /watchdog/status` (`results[{node,ok,err}]`) and `GET /watchdog/heap` (`min_free`, `wd_stack_free_min`). Expect `ok:true`; on failure `err` names the stage: `no_address` (not in SRP and not cached), `timeout_sigma1|sigma3|write` (unreachable or asleep), `case_nosharedroot` (IPK/fabric/node), `cert_*`, `sig_sigma2`, `write_status_NN` (IM status, 0x7e UNSUPPORTED_ACCESS means the ACL entry is missing).
4. Confirm independently: `chip-tool thermostat read occupied-heating-setpoint 8 1` from heatd shows 1800.
5. Switch back to `dry_run`, restore heartbeat parameters, and let heatd re-write its own setpoints.

## 3. What to capture if it fails

`idf.py monitor` is not needed: the failing stage is in `results[].err`. For wire level, run `tcpdump -i <thread/wpan iface or the BR's wifi> -w br.pcap 'udp port 5540'` on the Minix with the BR on the same link (Thread traffic is only visible on the Thread side: use the OTBR host's `wpan0` or a sniffer), and open it in Wireshark with the Matter dissector; Sigma1/2/3 and StatusReport are in the clear, only the TBE payloads and the session are encrypted.

## Known unverified points (need the real device)

- lwIP source address selection for the Thread netif (ML-EID vs OMR) and whether the thermostat answers to it; the client takes the non-link-local address from the SRP entry and tries the first two.
- Thread SED timing: the retransmit base is the device SII from its SRP TXT clamped to 2-60 s (default 3 s), 5 retransmissions with the Matter backoff; sleepy children behind the BR may need more.
- Devices that insist on session parameters in Sigma1 (none known); the client omits them.
- Certificate time validity is not checked (the BR has no wall clock), same as CHIP's "no real time" fallback without last-known-good time.
