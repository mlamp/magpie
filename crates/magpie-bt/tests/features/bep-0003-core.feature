Feature: BEP 3 — core BitTorrent protocol
  Reference: https://www.bittorrent.org/beps/bep_0003.html

  Scenario: handshake round-trips with cleared reserved bits
    Given a handshake with info-hash "AA" repeated 20 times and peer-id "BB" repeated 20 times
    When the handshake is encoded then decoded
    Then the decoded info-hash matches and the Fast extension bit is unset

  Scenario: Have message round-trips through the wire codec
    Given a Have message for piece 42
    When the wire codec encodes and decodes it
    Then the decoded piece index is 42

  Scenario: Request message canonical layout
    Given a Request for piece 1, offset 16384, length 16384
    When the wire codec encodes it
    Then the resulting frame begins with length-prefix 13 and message id 6

  Scenario: Bitfield round-trip preserves payload bytes
    Given a Bitfield containing bytes "FF 0F"
    When the wire codec encodes and decodes it
    Then the decoded bitfield bytes equal "FF 0F"
