Feature: BEP 10 — Extension Protocol
  Reference: https://www.bittorrent.org/beps/bep_0010.html

  Scenario: Handshake advertises BEP 10 via reserved byte 5 bit 0x10
    Given a handshake with the extension-protocol bit set
    When the handshake is encoded then decoded
    Then the extension-protocol bit is set on the decoded handshake

  Scenario: Extension handshake round-trips its m dict and supplemental fields
    Given an ExtensionHandshake with m={"ut_metadata":1,"ut_pex":2}, metadata_size=31415, client="test-client"
    When the bencoded payload is encoded then decoded
    Then the decoded m dict maps "ut_metadata"→1 and "ut_pex"→2
    And metadata_size is 31415
    And client is "test-client"

  Scenario: Decoder is lenient about unknown keys
    Given a handshake bencode payload containing a key the decoder does not recognise
    When the payload is decoded
    Then decoding succeeds and the unknown key is ignored

  Scenario: Decoder rejects a non-dict payload
    Given an extension-handshake payload that is not a bencoded dict
    When the payload is decoded
    Then decoding fails

  Scenario: Extension IDs of 0 are reserved for the handshake itself
    Given an inbound Message::Extended with id=0 after the initial handshake
    When the peer-conn dispatcher processes the message
    Then it is treated as a (possibly late) handshake update — not a payload dispatch

  Scenario: Per-peer ExtensionRegistry tracks local and remote IDs independently
    Given a registry constructed with local m={"ut_metadata":1,"ut_pex":2}
    When the registry records the remote handshake m={"ut_metadata":3,"ut_pex":4}
    Then local_id("ut_metadata") is 1 and remote_id("ut_metadata") is 3
    And local_name_for_id(1) returns "ut_metadata"
    And local_name_for_id(99) returns None

  Scenario: PeerConn exchanges extension handshakes immediately after the BEP 3 handshake
    Given a connected pair of peers each advertising the BEP 10 reserved bit
    When the post-handshake routine runs
    Then each side sends its ExtensionHandshake
    And inbound non-handshake messages received before the peer's handshake are processed normally
    And a missing peer handshake (timeout) does not kill the connection

  Scenario: Inbound extended payload size is capped at 1 MB
    Given an inbound Message::Extended with payload exceeding 1 MB
    When the peer-conn frame decoder processes it
    Then the connection rejects the frame
