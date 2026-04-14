Feature: BEP 6 — Fast extension
  Reference: https://www.bittorrent.org/beps/bep_0006.html

  Scenario: handshake advertises Fast extension via reserved byte 7 bit 0x04
    Given a handshake with the Fast extension bit set
    When the handshake is encoded then decoded
    Then the Fast extension bit is set

  Scenario: HaveAll round-trips as a single-byte payload frame
    Given a HaveAll message
    When the wire codec encodes and decodes it
    Then the decoded message is HaveAll

  Scenario: HaveNone round-trips as a single-byte payload frame
    Given a HaveNone message
    When the wire codec encodes and decodes it
    Then the decoded message is HaveNone

  Scenario: AllowedFast round-trips with a single piece index
    Given an AllowedFast message for piece 99
    When the wire codec encodes and decodes it
    Then the decoded AllowedFast piece index is 99

  Scenario: RejectRequest round-trips with the rejected block address
    Given a RejectRequest for piece 1, offset 16384, length 16384
    When the wire codec encodes and decodes it
    Then the decoded RejectRequest matches the original block address
