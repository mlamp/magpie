Feature: BEP 15 — UDP tracker protocol
  Reference: https://www.bittorrent.org/beps/bep_0015.html

  Scenario: CONNECT request carries the protocol magic first
    Given a fresh transaction_id 0xDEADBEEF
    When a CONNECT request is encoded
    Then the first 8 bytes equal 0x0000041727101980
    And bytes 8..12 encode action = 0 (CONNECT)
    And bytes 12..16 encode transaction_id = 0xDEADBEEF

  Scenario: CONNECT response must match the sent transaction_id
    Given a CONNECT response with transaction_id 0xCAFEBABE
    When the client expected transaction_id 0x12345678
    Then decoding returns a decode error

  Scenario: tracker error response propagates as a typed failure
    Given a UDP response with action = 3 (ERROR) and body "info_hash unknown"
    When the response is decoded
    Then decoding returns a tracker failure with message "info_hash unknown"

  Scenario: ANNOUNCE response peer list decodes from compact IPv4 form
    Given an ANNOUNCE response with interval 1800 and two compact IPv4 peers
    When the response is decoded
    Then the announce interval is 1800 seconds
    And the parsed peer list contains "10.0.0.1:6881"
    And the parsed peer list contains "192.168.1.5:6881"

  Scenario: retry timeout follows the BEP 15 curve (15s × 2^n, cap 3840s)
    When the retry timeout is computed for attempts 0, 1, 2, 7, 8, 20
    Then the timeouts are 15s, 30s, 60s, 1920s, 3840s, 3840s

  # Deferred: requires the high-level UDP-tracker client wrapper that
  # composes encode_connect / decode_announce / connection_id cache /
  # retry loop. Currently magpie ships only the codec primitives; the
  # client wrapper lands in a follow-up wired to UdpDemux. Scenario kept
  # as living documentation of the expected behaviour.
  @deferred
  Scenario: connection_id is refreshed every 60 seconds
    Given a tracker client that observed its last CONNECT reply 61 seconds ago
    When the next ANNOUNCE is prepared
    Then the client first re-issues CONNECT to refresh the connection_id
