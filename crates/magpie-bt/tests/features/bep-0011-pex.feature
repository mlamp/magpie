Feature: BEP 11 — Peer Exchange (ut_pex)
  Reference: https://www.bittorrent.org/beps/bep_0011.html

  Scenario: PexMessage round-trips IPv4 added/dropped with per-peer flags
    Given a PexMessage with two IPv4 added peers (one SEED, one REACHABLE) and one IPv4 dropped peer
    When the wire codec encodes and decodes it
    Then the decoded message equals the original

  Scenario: PexMessage round-trips IPv6 added/dropped with per-peer flags
    Given a PexMessage with one IPv6 added peer (SUPPORTS_UTP) and one IPv6 dropped peer
    When the wire codec encodes and decodes it
    Then the decoded message equals the original

  Scenario: PexMessage round-trips a mixed v4/v6 added+dropped batch
    Given a PexMessage carrying both IPv4 and IPv6 peers in added and dropped
    When the wire codec encodes and decodes it
    Then the decoded message preserves all addresses and flags

  Scenario: Missing added.f bytes default each peer's flags to 0
    Given a wire payload with one IPv4 added peer and no "added.f" key
    When the wire codec decodes it
    Then the decoded peer's flags are PexFlags(0)

  Scenario: Decoder rejects added.f length that does not match the peer count
    Given a payload with two IPv4 added peers and only one byte of added.f
    When the wire codec decodes it
    Then decoding fails with a Decode error mentioning "added.f"

  Scenario: Decoder rejects compact peer lists with truncated stride
    Given a payload whose "added" bytes length is not a multiple of 6
    When the wire codec decodes it
    Then decoding fails with InvalidCompactLength

  Scenario: Decoder rejects messages exceeding MAX_PEX_PEERS (200)
    Given a payload carrying MAX_PEX_PEERS + 1 added peers
    When the wire codec decodes it
    Then decoding fails with TooManyPeers

  Scenario: Outbound PEX is throttled to once every 60s per peer
    Given a torrent that has just sent a PEX round to peer P
    When PEX scheduling fires again less than 60s later
    Then no PEX message is sent to P

  Scenario: Inbound PEX from the same peer is rate-limited to once every 10s
    Given a torrent that just accepted a PEX message from peer P
    When P sends another PEX message within 10s
    Then the message is dropped without updating discovered peers

  Scenario: Outbound PEX is diff-based and capped per round
    Given a peer set that has gained 80 peers and lost 60 peers since the last PEX
    When the next PEX round is built for a remote peer
    Then the outbound message carries at most 50 added and 50 dropped entries

  Scenario: Private torrents must not exchange PEX
    Given a torrent session whose info dict carries "private": 1
    When PEX would otherwise schedule for any peer
    Then no ut_pex extension is advertised and no PEX message is sent

  Scenario: PEX-discovered peers are surfaced to the engine
    Given a torrent that received an inbound PEX with two new added peers
    When the engine drains pex_discovered
    Then both addresses are returned for connection follow-up
